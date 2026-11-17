//! Message preview (QoL): Discord-flavoured markdown parsing, emoji
//! shortcode expansion for display, and the 2000-character limit check.
//!
//! Pure functions, no I/O, fully unit-tested. The preview shows what
//! the message will look like; it never changes what gets sent.
//!
//! Shortcode table: a small hand-written list of common Unicode emoji
//! (general knowledge, e.g. `:fire:`). It is NOT copied from any
//! external emoji dataset. Unknown codes are left untouched.
//!
//! Note: shortcodes are expanded for PREVIEW ONLY. The sent content
//! keeps the exact text the user typed (Discord's API does not expand
//! `:name:` shortcodes server-side, so previewing the expansion while
//! sending the raw text would be misleading without the UI note that
//! says exactly that).

/// Discord's content limit for one message (characters, not bytes).
pub const DISCORD_CONTENT_LIMIT: usize = 2000;

/// Character count (Unicode scalar values). Emoji count as one each.
pub fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// True when the text is longer than Discord allows in one message.
pub fn over_limit(text: &str) -> bool {
    char_count(text) > DISCORD_CONTENT_LIMIT
}

/// One run of text with uniform formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
}

impl Segment {
    fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            bold: false,
            italic: false,
            strike: false,
            underline: false,
            code: false,
        }
    }
}

/// Parse Discord-flavoured inline markdown into segments.
///
/// Supported: `**bold**`, `*italic*`, `_italic_`, `~~strike~~`,
/// `__underline__`, `` `code` ``. Code spans win over everything
/// inside them. Unmatched delimiters stay literal. Single `*` and `_`
/// need a word boundary (start/end, whitespace, or punctuation) on
/// both sides, so `hello_world` and `2*3*4` are not mangled.
pub fn parse_markdown(text: &str) -> Vec<Segment> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;

    let flush = |plain: &mut String, out: &mut Vec<Segment>| {
        if !plain.is_empty() {
            out.push(Segment::plain(plain));
            plain.clear();
        }
    };

    // Find a closing delimiter starting at `from`, returning its index.
    // Content must be non-empty. Newlines are allowed inside.
    let find_close = |chars: &[char], from: usize, delim: &[char]| -> Option<usize> {
        if from >= chars.len() {
            return None;
        }
        let mut j = from;
        while j + delim.len() <= chars.len() {
            if chars[j..j + delim.len()] == *delim {
                return Some(j);
            }
            j += 1;
        }
        None
    };

    let is_boundary = |chars: &[char], idx: usize, before: bool| -> bool {
        if before {
            idx == 0 || !chars[idx - 1].is_alphanumeric()
        } else {
            idx >= chars.len() || !chars[idx].is_alphanumeric()
        }
    };

    while i < chars.len() {
        let rest = &chars[i..];
        // Code span: `...` (single backtick; content verbatim).
        if rest[0] == '`' {
            if let Some(close) = find_close(&chars, i + 1, &['`']) {
                if close > i + 1 {
                    flush(&mut plain, &mut out);
                    let content: String = chars[i + 1..close].iter().collect();
                    out.push(Segment {
                        text: content,
                        code: true,
                        ..Segment::plain("")
                    });
                    i = close + 1;
                    continue;
                }
            }
            plain.push('`');
            i += 1;
            continue;
        }
        // Two-char delimiters: **, ~~, __.
        if rest.len() >= 2 {
            let two = [rest[0], rest[1]];
            let kind = match two {
                ['*', '*'] => Some("bold"),
                ['~', '~'] => Some("strike"),
                ['_', '_'] => Some("underline"),
                _ => None,
            };
            if let Some(kind) = kind {
                if let Some(close) = find_close(&chars, i + 2, &two) {
                    if close > i + 2 {
                        flush(&mut plain, &mut out);
                        let content: String = chars[i + 2..close].iter().collect();
                        // Recurse for nesting like **bold *and italic***?
                        // No: keep v1 flat. Inner markers stay literal,
                        // which is predictable and never mangles text.
                        let mut seg = Segment::plain(&content);
                        match kind {
                            "bold" => seg.bold = true,
                            "strike" => seg.strike = true,
                            "underline" => seg.underline = true,
                            _ => {}
                        }
                        out.push(seg);
                        i = close + 2;
                        continue;
                    }
                }
                // Unmatched: emit the two chars literally.
                plain.push(rest[0]);
                plain.push(rest[1]);
                i += 2;
                continue;
            }
        }
        // Single-char italic: *x* or _x_, with word boundaries.
        if rest[0] == '*' || rest[0] == '_' {
            let delim = rest[0];
            let after_close_ok = |close: usize| -> bool {
                // Char after the closing delimiter must be a boundary.
                is_boundary(&chars, close + 1, false)
            };
            if is_boundary(&chars, i, true) {
                if let Some(close) = find_close(&chars, i + 1, &[delim]) {
                    if close > i + 1 && after_close_ok(close) {
                        // Content must not start/end with whitespace
                        // (Discord does not format "* foo *").
                        let content: String = chars[i + 1..close].iter().collect();
                        if !content.starts_with(char::is_whitespace)
                            && !content.ends_with(char::is_whitespace)
                        {
                            flush(&mut plain, &mut out);
                            let mut seg = Segment::plain(&content);
                            seg.italic = true;
                            out.push(seg);
                            i = close + 1;
                            continue;
                        }
                    }
                }
            }
            plain.push(delim);
            i += 1;
            continue;
        }
        plain.push(rest[0]);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

/// Expand `:shortcode:` sequences to their Unicode emoji, for preview
/// display only. Unknown codes are left as-is. Only lowercase/digit/
/// underscore/plus/dash names are recognised, so times (`12:30`) and
/// URLs (`https://…`) are untouched.
pub fn expand_shortcodes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == ':' {
            // Scan a candidate name.
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_ascii_lowercase()
                    || chars[j].is_ascii_digit()
                    || matches!(chars[j], '_' | '+' | '-'))
            {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && chars[j] == ':' {
                let name: String = chars[i + 1..j].iter().collect();
                if let Some(emoji) = shortcode_table(&name) {
                    out.push_str(emoji);
                    i = j + 1;
                    continue;
                }
            }
            out.push(':');
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Hand-written shortcode table (common emoji only). Written from
/// general knowledge of the standard shortcode names; no external
/// dataset is used.
/// Emoji picker list (Q10): one canonical shortcode per character,
/// in picker order. Characters match `shortcode_table`; the picker
/// inserts the character itself (not the shortcode).
pub const EMOJI_PICKER_LIST: &[(&str, &str)] = &[
    ("smile", "😊"),
    ("grin", "😁"),
    ("joy", "😂"),
    ("wink", "😉"),
    ("heart", "❤️"),
    ("fire", "🔥"),
    ("star", "⭐"),
    ("sparkles", "✨"),
    ("rocket", "🚀"),
    ("tada", "🎉"),
    ("thumbsup", "👍"),
    ("thumbsdown", "👎"),
    ("ok_hand", "👌"),
    ("pray", "🙏"),
    ("clap", "👏"),
    ("muscle", "💪"),
    ("eyes", "👀"),
    ("check", "✅"),
    ("x", "❌"),
    ("warning", "⚠️"),
    ("question", "❓"),
    ("exclamation", "❗"),
    ("bell", "🔔"),
    ("bulb", "💡"),
    ("book", "📖"),
    ("calendar", "📅"),
    ("clock", "🕐"),
    ("megaphone", "📢"),
    ("speech_balloon", "💬"),
    ("gift", "🎁"),
    ("coffee", "☕"),
    ("sun", "☀️"),
    ("moon", "🌙"),
    ("wave", "👋"),
    ("100", "💯"),
    ("zap", "⚡"),
];

fn shortcode_table(name: &str) -> Option<&'static str> {
    Some(match name {
        "smile" | "smiling_face" => "😊",
        "grin" | "grinning" => "😁",
        "laugh" | "joy" => "😂",
        "wink" => "😉",
        "heart" | "red_heart" => "❤️",
        "fire" => "🔥",
        "star" => "⭐",
        "sparkles" => "✨",
        "rocket" => "🚀",
        "tada" | "party" => "🎉",
        "thumbsup" | "+1" => "👍",
        "thumbsdown" | "-1" => "👎",
        "ok_hand" => "👌",
        "pray" => "🙏",
        "clap" => "👏",
        "muscle" => "💪",
        "eyes" => "👀",
        "check" | "white_check_mark" => "✅",
        "x" | "cross" => "❌",
        "warning" => "⚠️",
        "question" => "❓",
        "exclamation" | "bang" => "❗",
        "bell" => "🔔",
        "bulb" => "💡",
        "book" => "📖",
        "calendar" => "📅",
        "clock" => "🕐",
        "megaphone" => "📣",
        "speech_balloon" => "💬",
        "gift" => "🎁",
        "coffee" => "☕",
        "sun" => "☀️",
        "moon" => "🌙",
        "wave" => "👋",
        "100" => "💯",
        "zap" => "⚡",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Segment {
        Segment::plain(text)
    }

    #[test]
    fn markdown_bold_italic_strike_underline_code() {
        let segs = parse_markdown("a **b** c *d* e ~~f~~ g __h__ i `j` k");
        assert!(segs.iter().any(|s| s.text == "b" && s.bold));
        assert!(segs.iter().any(|s| s.text == "d" && s.italic));
        assert!(segs.iter().any(|s| s.text == "f" && s.strike));
        assert!(segs.iter().any(|s| s.text == "h" && s.underline));
        assert!(segs.iter().any(|s| s.text == "j" && s.code));
        // Plain parts survive.
        assert!(segs.iter().any(|s| s.text.contains('a') && !s.bold));
    }

    #[test]
    fn markdown_unmatched_stays_literal() {
        let segs = parse_markdown("**open *half `code");
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "**open *half `code");
        assert!(segs.iter().all(|s| !s.bold && !s.italic && !s.code));
    }

    #[test]
    fn markdown_does_not_mangle_words_or_math() {
        let segs = parse_markdown("hello_world and 2*3*4");
        assert_eq!(segs, vec![seg("hello_world and 2*3*4")]);
    }

    #[test]
    fn markdown_code_wins_over_markers() {
        let segs = parse_markdown("`a **b** c`");
        assert_eq!(segs.len(), 1);
        assert!(segs[0].code);
        assert_eq!(segs[0].text, "a **b** c");
    }

    #[test]
    fn markdown_underscore_italic_needs_boundaries() {
        let segs = parse_markdown("_ok_ and x_y_z");
        assert!(segs.iter().any(|s| s.text == "ok" && s.italic));
        assert!(segs.iter().any(|s| s.text.contains("x_y_z")));
    }

    #[test]
    fn shortcodes_expand_known_only() {
        assert_eq!(expand_shortcodes("hi :fire: there"), "hi 🔥 there");
        assert_eq!(
            expand_shortcodes(":unknown_code: stays"),
            ":unknown_code: stays"
        );
        assert_eq!(expand_shortcodes("at 12:30 sharp"), "at 12:30 sharp");
        assert_eq!(
            expand_shortcodes("see https://example.com/x"),
            "see https://example.com/x"
        );
    }

    #[test]
    fn char_count_uses_chars_not_bytes() {
        assert_eq!(char_count("🔥🔥"), 2);
        assert_eq!(char_count("héllo"), 5);
        assert!(!over_limit(&"a".repeat(2000)));
        assert!(over_limit(&"a".repeat(2001)));
    }
}
