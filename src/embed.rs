//! Discord rich embed: model, validation, and Discord payload.
//!
//! Limits are taken from Discord's official docs (verified
//! 2026-11-17 against docs.discord.com/developers, Message
//! Resource "Embed Limits"): title 256, description 4096, at most
//! 25 fields, field name 256, field value 1024, footer text 2048,
//! author name 256, total across all embeds ≤ 6000 characters
//! (content not counted), at most 10 embeds per message. Violations
//! return 400. The color range 0..=0xFFFFFF is an APP decision —
//! the docs only say "integer". URLs (title link, image,
//! thumbnail) must be https — an APP rule, matching the avatar
//! rule: Discord fetches these server-side, and plain-http or
//! relative URLs would fail silently or leak the request.
//!
//! A scenario may carry up to 10 embeds (idea #8); the 6000-char
//! total is enforced ACROSS all of them by `validate_embeds`.
//! There is deliberately no `timestamp` field: a fixed timestamp
//! on a recurring message would go stale, and a send-time stamp
//! belongs in the send layer, not the stored model.

use serde::{Deserialize, Serialize};

pub const EMBED_TITLE_LIMIT: usize = 256;
pub const EMBED_DESCRIPTION_LIMIT: usize = 4096;
pub const EMBED_MAX_FIELDS: usize = 25;
pub const EMBED_FIELD_NAME_LIMIT: usize = 256;
pub const EMBED_FIELD_VALUE_LIMIT: usize = 1024;
pub const EMBED_FOOTER_LIMIT: usize = 2048;
pub const EMBED_AUTHOR_LIMIT: usize = 256;
pub const EMBED_TOTAL_LIMIT: usize = 6000;
pub const EMBED_MAX_COUNT: usize = 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedField {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Embed {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Sidebar color as 0xRRGGBB (None = Discord default).
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub author_name: String,
    #[serde(default)]
    pub fields: Vec<EmbedField>,
    #[serde(default)]
    pub footer_text: String,
    /// Title link (https only, empty = no link). Clicking the
    /// title opens this URL.
    #[serde(default)]
    pub url: String,
    /// Large image shown at the bottom (https only, empty = none).
    #[serde(default)]
    pub image_url: String,
    /// Small image shown at the top-right (https only, empty = none).
    #[serde(default)]
    pub thumbnail_url: String,
}

impl Embed {
    /// True when nothing would be visible (no title, description,
    /// fields, footer, author, or image). An all-empty embed is
    /// invalid — except that an image-only embed DOES render.
    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty()
            && self.description.trim().is_empty()
            && self.author_name.trim().is_empty()
            && self.footer_text.trim().is_empty()
            && self.image_url.trim().is_empty()
            && self.thumbnail_url.trim().is_empty()
            && self.fields.is_empty()
    }

    /// Total characters counted by Discord's 6000 limit: title +
    /// description + field names/values + footer + author.
    pub fn total_chars(&self) -> usize {
        let mut total = self.title.chars().count()
            + self.description.chars().count()
            + self.author_name.chars().count()
            + self.footer_text.chars().count();
        for f in &self.fields {
            total += f.name.chars().count() + f.value.chars().count();
        }
        total
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.is_empty() {
            return Err("embed đang trống — thêm tiêu đề, mô tả hoặc trường".into());
        }
        if self.title.chars().count() > EMBED_TITLE_LIMIT {
            return Err(format!(
                "tiêu đề embed vượt giới hạn {EMBED_TITLE_LIMIT} ký tự"
            ));
        }
        if self.description.chars().count() > EMBED_DESCRIPTION_LIMIT {
            return Err(format!(
                "mô tả embed vượt giới hạn {EMBED_DESCRIPTION_LIMIT} ký tự"
            ));
        }
        if self.author_name.chars().count() > EMBED_AUTHOR_LIMIT {
            return Err(format!(
                "tác giả embed vượt giới hạn {EMBED_AUTHOR_LIMIT} ký tự"
            ));
        }
        if self.footer_text.chars().count() > EMBED_FOOTER_LIMIT {
            return Err(format!(
                "chân trang embed vượt giới hạn {EMBED_FOOTER_LIMIT} ký tự"
            ));
        }
        if self.fields.len() > EMBED_MAX_FIELDS {
            return Err(format!("embed có nhiều hơn {EMBED_MAX_FIELDS} trường"));
        }
        for (idx, f) in self.fields.iter().enumerate() {
            if f.name.trim().is_empty() || f.value.trim().is_empty() {
                return Err(format!("trường {} của embed cần tên và giá trị", idx + 1));
            }
            if f.name.chars().count() > EMBED_FIELD_NAME_LIMIT {
                return Err(format!(
                    "tên trường {} của embed vượt giới hạn {EMBED_FIELD_NAME_LIMIT} ký tự",
                    idx + 1
                ));
            }
            if f.value.chars().count() > EMBED_FIELD_VALUE_LIMIT {
                return Err(format!(
                    "giá trị trường {} của embed vượt giới hạn {EMBED_FIELD_VALUE_LIMIT} ký tự",
                    idx + 1
                ));
            }
        }
        if let Some(color) = self.color {
            if color > 0xFF_FFFF {
                return Err("màu embed phải là 0x000000..0xFFFFFF".into());
            }
        }
        // URLs: https only, and more than the bare scheme (the
        // same rule as destination avatar URLs).
        for (what, url) in [
            ("link", &self.url),
            ("image", &self.image_url),
            ("thumbnail", &self.thumbnail_url),
        ] {
            let trimmed = url.trim();
            if !trimmed.is_empty()
                && (!trimmed.starts_with("https://") || trimmed.len() <= "https://".len())
            {
                return Err(format!("URL {what} của embed phải bắt đầu bằng https://"));
            }
        }
        if self.total_chars() > EMBED_TOTAL_LIMIT {
            return Err(format!(
                "tổng số ký tự của embed là {}, vượt giới hạn {EMBED_TOTAL_LIMIT} ký tự của Discord",
                self.total_chars()
            ));
        }
        Ok(())
    }

    /// Discord payload shape for one embed. Empty parts are omitted.
    /// `type` is always "rich" for webhook embeds.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("rich".to_string()),
        );
        if !self.title.trim().is_empty() {
            obj.insert(
                "title".to_string(),
                serde_json::Value::String(self.title.clone()),
            );
        }
        if !self.description.trim().is_empty() {
            obj.insert(
                "description".to_string(),
                serde_json::Value::String(self.description.clone()),
            );
        }
        if !self.url.trim().is_empty() {
            obj.insert(
                "url".to_string(),
                serde_json::Value::String(self.url.clone()),
            );
        }
        if let Some(color) = self.color {
            obj.insert("color".to_string(), serde_json::Value::Number(color.into()));
        }
        if !self.author_name.trim().is_empty() {
            obj.insert(
                "author".to_string(),
                serde_json::json!({ "name": self.author_name }),
            );
        }
        if !self.fields.is_empty() {
            let fields: Vec<serde_json::Value> = self
                .fields
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "name": f.name,
                        "value": f.value,
                        "inline": f.inline,
                    })
                })
                .collect();
            obj.insert("fields".to_string(), serde_json::Value::Array(fields));
        }
        if !self.footer_text.trim().is_empty() {
            obj.insert(
                "footer".to_string(),
                serde_json::json!({ "text": self.footer_text }),
            );
        }
        if !self.thumbnail_url.trim().is_empty() {
            obj.insert(
                "thumbnail".to_string(),
                serde_json::json!({ "url": self.thumbnail_url }),
            );
        }
        if !self.image_url.trim().is_empty() {
            obj.insert(
                "image".to_string(),
                serde_json::json!({ "url": self.image_url }),
            );
        }
        serde_json::Value::Object(obj)
    }
}

/// Validate a whole message's embeds: at most 10, each embed
/// valid on its own, and the 6000-character total enforced
/// ACROSS all of them (Discord counts the sum, not each embed).
pub fn validate_embeds(embeds: &[Embed]) -> Result<(), String> {
    if embeds.len() > EMBED_MAX_COUNT {
        return Err(format!(
            "tin nhắn có {} embed, vượt giới hạn {EMBED_MAX_COUNT} embed của Discord",
            embeds.len()
        ));
    }
    let mut total = 0;
    for (idx, embed) in embeds.iter().enumerate() {
        embed
            .validate()
            .map_err(|e| format!("embed {}: {e}", idx + 1))?;
        total += embed.total_chars();
    }
    if total > EMBED_TOTAL_LIMIT {
        return Err(format!(
            "tổng số ký tự các embed là {total}, vượt giới hạn {EMBED_TOTAL_LIMIT} ký tự của Discord"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_catches_empty_and_overlong() {
        let empty = Embed::default();
        assert!(empty.validate().is_err());
        let mut e = Embed {
            title: "Hi".to_string(),
            ..Embed::default()
        };
        assert!(e.validate().is_ok());
        e.title = "a".repeat(257);
        assert!(e.validate().is_err());
        e.title = "Hi".to_string();
        e.fields = vec![EmbedField {
            name: "n".to_string(),
            value: "v".to_string(),
            inline: false,
        }];
        assert!(e.validate().is_ok());
        e.fields[0].value = "a".repeat(1025);
        assert!(e.validate().is_err());
        e.fields[0].value = "v".to_string();
        e.color = Some(0x1_000_000);
        assert!(e.validate().is_err());
        e.color = Some(0x58_65F2);
        assert!(e.validate().is_ok());
    }

    #[test]
    fn total_limit_counts_all_parts() {
        let e = Embed {
            description: "a".repeat(4000),
            fields: vec![EmbedField {
                name: "n".repeat(256),
                value: "v".repeat(1024),
                inline: false,
            }],
            footer_text: "f".repeat(1000),
            ..Embed::default()
        };
        assert!(e.total_chars() > EMBED_TOTAL_LIMIT);
        assert!(e.validate().is_err());
    }

    #[test]
    fn to_json_skips_empty_parts() {
        let e = Embed {
            title: "T".to_string(),
            color: Some(0xFF_0000),
            ..Embed::default()
        };
        let v = e.to_json();
        assert_eq!(v["type"], "rich");
        assert_eq!(v["title"], "T");
        assert_eq!(v["color"], 0xFF_0000);
        assert!(v.get("description").is_none());
        assert!(v.get("fields").is_none());
    }

    #[test]
    fn urls_must_be_https_and_appear_in_payload() {
        let mut e = Embed {
            title: "T".to_string(),
            ..Embed::default()
        };
        e.url = "http://example.com".to_string();
        assert!(e.validate().is_err());
        e.url = "https://".to_string();
        assert!(e.validate().is_err());
        e.url = "https://example.com/page".to_string();
        e.image_url = "https://cdn.example.com/a.png".to_string();
        e.thumbnail_url = "https://cdn.example.com/t.png".to_string();
        assert!(e.validate().is_ok());
        let v = e.to_json();
        assert_eq!(v["url"], "https://example.com/page");
        assert_eq!(v["image"]["url"], "https://cdn.example.com/a.png");
        assert_eq!(v["thumbnail"]["url"], "https://cdn.example.com/t.png");
        // An image-only embed is not empty: it renders.
        let img_only = Embed {
            image_url: "https://cdn.example.com/a.png".to_string(),
            ..Embed::default()
        };
        assert!(!img_only.is_empty());
        assert!(img_only.validate().is_ok());
    }

    #[test]
    fn validate_embeds_enforces_count_and_shared_total() {
        let one = |len: usize| Embed {
            description: "a".repeat(len),
            ..Embed::default()
        };
        assert!(validate_embeds(&[one(10), one(20)]).is_ok());
        // Each under 6000, but the sum is over: rejected.
        assert!(validate_embeds(&[one(4000), one(3000)]).is_err());
        // 11 embeds: rejected even when tiny.
        let many = vec![one(1); 11];
        assert!(validate_embeds(&many).is_err());
        // An empty embed inside the list is rejected with its index.
        let err = validate_embeds(&[one(1), Embed::default()]).unwrap_err();
        assert!(err.contains("embed 2"), "{err}");
    }
}
