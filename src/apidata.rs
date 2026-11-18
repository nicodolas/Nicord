//! External data via HTTP (idea #5): `UtilityCallApi` nodes fetch
//! JSON over HTTPS and feed one field into a template variable.
//!
//! Security posture (the URL is user-configured, for their own
//! machine — the user is responsible for what they point Nicord
//! at, but the fetch itself is fenced):
//!
//! - HTTPS only. HTTP, userinfo, and malformed URLs are rejected
//!   before any request.
//! - Optional host allowlist: when non-empty, the URL's host must
//!   match it (exact, or a leading-dot suffix entry).
//! - Per-fetch timeout (default 10 s, 1–60 s) and a 1 MiB
//!   response cap enforced while streaming — a huge or hanging
//!   response can never stall the engine or exhaust memory.
//! - No credentials are ever attached (no auth headers, no
//!   cookies); the URL itself may carry an API key, so it is
//!   stored via the secret store, never logged, and never echoed
//!   in error messages — errors name the host at most.
//! - The response body is never logged or echoed: only the
//!   extracted field value reaches the template.

use crate::discord::reqwest_error;

/// Default per-fetch timeout in seconds.
pub const DEFAULT_API_TIMEOUT_SECS: u64 = 10;
/// Maximum response body accepted, in bytes (1 MiB).
pub const API_MAX_BYTES: usize = 1024 * 1024;

/// Extract one value from a JSON document via a dot path
/// (`weather.temp`, array indices as numbers: `items.0.name`).
/// An empty path returns the whole document. Any missing key,
/// out-of-range index, or malformed segment returns None.
pub fn extract_json_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    if path.is_empty() {
        return Some(value);
    }
    let mut current = value;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        if let Ok(index) = segment.parse::<usize>() {
            // Numeric segments index arrays only — an object key
            // that happens to be numeric is not matched, so a
            // path cannot silently mean two things.
            current = current.as_array()?.get(index)?;
        } else {
            current = current.as_object()?.get(segment)?;
        }
    }
    Some(current)
}

/// Render an extracted JSON value as template text: strings
/// as-is, numbers/booleans in plain form, null as empty, and
/// objects/arrays as compact JSON.
pub fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Parse + validate an API URL. Returns the lowercased host
/// (without any port) for allowlist checks. Strict on purpose:
/// only HTTPS, no userinfo, no whitespace/fragment, host
/// required; an explicit port must be digits-only. Error
/// messages NEVER echo the input, because it may contain an API
/// key.
pub fn parse_api_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("API URL is empty".into());
    }
    if s.len() > 2048 {
        return Err("API URL is too long".into());
    }
    if s.contains('#') || s.chars().any(|c| c.is_ascii_whitespace()) {
        return Err("API URL must look like https://host/path".into());
    }
    // Byte-compare the scheme (see parse_webhook_url for why
    // slicing is avoided before the match).
    let prefix_ok = s.len() >= 8 && s.as_bytes()[..8].eq_ignore_ascii_case(b"https://");
    if !prefix_ok {
        return Err("API URL must use https://".into());
    }
    let rest = &s[8..];
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err("API URL must look like https://host/path".into());
    }
    // Split an explicit port (digits only); IPv6 literals are
    // rejected — bracketed hosts are out of scope for v1.
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err("API URL port must be numeric".into());
            }
            if p.parse::<u16>().is_err() {
                return Err("API URL port is out of range".into());
            }
            (h, Some(p))
        }
        None => (authority, None),
    };
    let _ = port;
    if host.is_empty() || host.contains('[') || host.contains(']') {
        return Err("API URL must look like https://host/path".into());
    }
    Ok(host.to_ascii_lowercase())
}

/// Whether `host` (lowercased) passes the allowlist. An empty
/// allowlist allows everything. Entries match exactly, or — when
/// an entry starts with `.` — as a suffix (`.example.com`
/// matches `api.example.com` but not `example.com` itself).
/// Entries are trimmed and lowercased before comparing.
pub fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    let host = host.trim().to_ascii_lowercase();
    allowlist.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        if entry.is_empty() {
            return false;
        }
        if let Some(suffix) = entry.strip_prefix('.') {
            !suffix.is_empty() && host.ends_with(&format!(".{suffix}")) && host != suffix
        } else {
            host == entry
        }
    })
}

/// Fetch one field from a JSON HTTPS endpoint. The URL is
/// validated (HTTPS + allowlist) before any request, the body is
/// capped while streaming, and no part of the URL or body ever
/// appears in an error message.
pub async fn fetch_api_value(
    url: &str,
    field_path: &str,
    timeout_secs: u64,
    max_bytes: usize,
    allowlist: &[String],
) -> Result<String, String> {
    let host = parse_api_url(url)?;
    if !host_allowed(&host, allowlist) {
        return Err(format!("host '{host}' is not in the API allowlist"));
    }
    let client = reqwest::Client::builder()
        .user_agent("Nicord/0.1.0 (personal use)")
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;
    let mut resp = client
        .get(url.trim())
        .send()
        .await
        .map_err(|e| reqwest_error("network error", e).to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("API returned HTTP {status}"));
    }
    // Stream with a hard cap: a response over the cap is refused
    // as soon as it crosses it, without buffering the rest.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > max_bytes {
                    return Err(format!(
                        "API response is over the {} cap",
                        crate::scheduler::format_bytes(max_bytes as u64)
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(reqwest_error("network error", e).to_string()),
        }
    }
    let doc: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|_| "API response is not valid JSON".to_string())?;
    let value = extract_json_path(&doc, field_path)
        .ok_or_else(|| format!("field '{field_path}' was not found in the API response"))?;
    Ok(json_value_to_string(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> serde_json::Value {
        serde_json::json!({
            "weather": { "temp": 21.5, "city": "Hanoi" },
            "items": [{ "name": "a" }, { "name": "b" }],
            "count": 3,
            "ok": true,
            "nothing": null,
            "nested": { "deep": { "list": [10, 20] } }
        })
    }

    #[test]
    fn json_path_extracts_nested_fields_and_indices() {
        let d = doc();
        assert_eq!(
            extract_json_path(&d, "weather.temp"),
            Some(&serde_json::json!(21.5))
        );
        assert_eq!(
            extract_json_path(&d, "items.1.name"),
            Some(&serde_json::json!("b"))
        );
        assert_eq!(
            extract_json_path(&d, "nested.deep.list.0"),
            Some(&serde_json::json!(10))
        );
        assert_eq!(extract_json_path(&d, ""), Some(&d));
        // Missing keys, bad indices, and malformed paths -> None.
        assert_eq!(extract_json_path(&d, "weather.nope"), None);
        assert_eq!(extract_json_path(&d, "items.5"), None);
        assert_eq!(extract_json_path(&d, "items.x"), None);
        assert_eq!(extract_json_path(&d, "count.0"), None);
        assert_eq!(extract_json_path(&d, "weather..temp"), None);
        assert_eq!(extract_json_path(&d, ".weather"), None);
        assert_eq!(extract_json_path(&d, "weather."), None);
        // A numeric segment never matches an object key.
        let obj = serde_json::json!({"0": "zero"});
        assert_eq!(extract_json_path(&obj, "0"), None);
    }

    #[test]
    fn json_values_render_as_template_text() {
        assert_eq!(json_value_to_string(&serde_json::json!("hi")), "hi");
        assert_eq!(json_value_to_string(&serde_json::json!(42)), "42");
        assert_eq!(json_value_to_string(&serde_json::json!(21.5)), "21.5");
        assert_eq!(json_value_to_string(&serde_json::json!(true)), "true");
        assert_eq!(json_value_to_string(&serde_json::Value::Null), "");
        assert_eq!(
            json_value_to_string(&serde_json::json!({"a": 1})),
            "{\"a\":1}"
        );
        assert_eq!(json_value_to_string(&serde_json::json!([1, 2])), "[1,2]");
    }

    #[test]
    fn api_url_parsing_is_strict_about_scheme_and_shape() {
        assert_eq!(
            parse_api_url("https://api.example.com/v1/weather?key=SECRET").unwrap(),
            "api.example.com"
        );
        assert_eq!(
            parse_api_url("HTTPS://API.Example.COM/x").unwrap(),
            "api.example.com"
        );
        assert_eq!(
            parse_api_url("https://localhost:8080/data").unwrap(),
            "localhost"
        );
        assert_eq!(
            parse_api_url("https://127.0.0.1/data").unwrap(),
            "127.0.0.1"
        );
        // Errors never echo the input (it may carry an API key).
        for bad in [
            "http://api.example.com/x",
            "https://user:pass@api.example.com/x",
            "https://api.example.com:abc/x",
            "https://api.example.com:99999/x",
            "https:///path",
            "https://api.example.com/x y",
            "https://api.example.com/x#frag",
            "https://[::1]/x",
            "",
            "not a url",
        ] {
            let err = parse_api_url(bad).unwrap_err();
            assert!(!err.contains("SECRET"), "error leaks input: {err}");
            assert!(!err.contains(bad) || bad.is_empty(), "error: {err}");
        }
    }

    #[test]
    fn allowlist_matching_covers_exact_suffix_and_empty() {
        let list = vec!["api.example.com".to_string(), ".trusted.org".to_string()];
        assert!(host_allowed("api.example.com", &list));
        assert!(host_allowed("API.EXAMPLE.COM", &list));
        assert!(host_allowed("data.trusted.org", &list));
        assert!(!host_allowed("trusted.org", &list));
        assert!(!host_allowed("eviltrusted.org", &list));
        assert!(!host_allowed("other.com", &list));
        assert!(host_allowed("anything.example", &[]));
        assert!(host_allowed(
            "x.com",
            &[String::new(), "  ".to_string(), "x.com".to_string()]
        ));
    }
}
