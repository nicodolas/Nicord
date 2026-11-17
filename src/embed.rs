//! Discord rich embed: model, validation, and Discord payload.
//!
//! Limits are taken from Discord's official docs (verified
//! 2026-11-17 against docs.discord.com/developers, Message
//! Resource "Embed Limits"): title 256, description 4096, at most
//! 25 fields, field name 256, field value 1024, footer text 2048,
//! author name 256, total across all embeds ≤ 6000 characters
//! (content not counted), at most 10 embeds per message. Violations
//! return 400. The color range 0..=0xFFFFFF is an APP decision —
//! the docs only say "integer". Nicord sends at most ONE embed per
//! message in v1 (the composer edits one embed per scenario).

use serde::{Deserialize, Serialize};

pub const EMBED_TITLE_LIMIT: usize = 256;
pub const EMBED_DESCRIPTION_LIMIT: usize = 4096;
pub const EMBED_MAX_FIELDS: usize = 25;
pub const EMBED_FIELD_NAME_LIMIT: usize = 256;
pub const EMBED_FIELD_VALUE_LIMIT: usize = 1024;
pub const EMBED_FOOTER_LIMIT: usize = 2048;
pub const EMBED_AUTHOR_LIMIT: usize = 256;
pub const EMBED_TOTAL_LIMIT: usize = 6000;

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
}

impl Embed {
    /// True when nothing would be visible (no title, description,
    /// fields, footer, or author). An all-empty embed is invalid.
    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty()
            && self.description.trim().is_empty()
            && self.author_name.trim().is_empty()
            && self.footer_text.trim().is_empty()
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
            return Err("embed is empty — add a title, description, or field".into());
        }
        if self.title.chars().count() > EMBED_TITLE_LIMIT {
            return Err(format!(
                "embed title is over the {EMBED_TITLE_LIMIT}-character limit"
            ));
        }
        if self.description.chars().count() > EMBED_DESCRIPTION_LIMIT {
            return Err(format!(
                "embed description is over the {EMBED_DESCRIPTION_LIMIT}-character limit"
            ));
        }
        if self.author_name.chars().count() > EMBED_AUTHOR_LIMIT {
            return Err(format!(
                "embed author is over the {EMBED_AUTHOR_LIMIT}-character limit"
            ));
        }
        if self.footer_text.chars().count() > EMBED_FOOTER_LIMIT {
            return Err(format!(
                "embed footer is over the {EMBED_FOOTER_LIMIT}-character limit"
            ));
        }
        if self.fields.len() > EMBED_MAX_FIELDS {
            return Err(format!("embed has more than {EMBED_MAX_FIELDS} fields"));
        }
        for (idx, f) in self.fields.iter().enumerate() {
            if f.name.trim().is_empty() || f.value.trim().is_empty() {
                return Err(format!("embed field {} needs a name and a value", idx + 1));
            }
            if f.name.chars().count() > EMBED_FIELD_NAME_LIMIT {
                return Err(format!(
                    "embed field {} name is over the {EMBED_FIELD_NAME_LIMIT}-character limit",
                    idx + 1
                ));
            }
            if f.value.chars().count() > EMBED_FIELD_VALUE_LIMIT {
                return Err(format!(
                    "embed field {} value is over the {EMBED_FIELD_VALUE_LIMIT}-character limit",
                    idx + 1
                ));
            }
        }
        if let Some(color) = self.color {
            if color > 0xFF_FFFF {
                return Err("embed color must be 0x000000..0xFFFFFF".into());
            }
        }
        if self.total_chars() > EMBED_TOTAL_LIMIT {
            return Err(format!(
                "embed total is {} characters, over Discord's {EMBED_TOTAL_LIMIT}-character limit",
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
        serde_json::Value::Object(obj)
    }
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
}
