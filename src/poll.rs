//! Discord poll attached to a scheduled message (idea #3).
//!
//! Limits are taken from Discord's official docs (Message
//! Resource, "Poll Object"): question ≤ 300 characters, 2–10
//! answers, each answer ≤ 55 characters, duration 1 hour – 7
//! days. Violations return 400. The `poll` field is accepted by
//! both the execute-webhook body and the create-message body.
//!
//! What Nicord cannot do: read poll RESULTS. Webhooks have no
//! read API, and the user-token path does not read messages —
//! so results are visible only inside Discord. The composer
//! says so plainly. A poll may accompany empty content
//! (Discord requires content OR embeds OR a poll).

use serde::{Deserialize, Serialize};

pub const POLL_QUESTION_LIMIT: usize = 300;
pub const POLL_ANSWER_LIMIT: usize = 55;
pub const POLL_MIN_ANSWERS: usize = 2;
pub const POLL_MAX_ANSWERS: usize = 10;
pub const POLL_MIN_DURATION_HOURS: u32 = 1;
pub const POLL_MAX_DURATION_HOURS: u32 = 168; // 7 days
pub const POLL_DEFAULT_DURATION_HOURS: u32 = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub answers: Vec<String>,
    #[serde(default = "default_poll_duration")]
    pub duration_hours: u32,
    #[serde(default)]
    pub allow_multiselect: bool,
}

fn default_poll_duration() -> u32 {
    POLL_DEFAULT_DURATION_HOURS
}

impl Default for Poll {
    /// Two empty answer slots ready to fill, 24 h duration.
    /// A default poll does NOT pass `validate()` until the
    /// question and answers are filled in — the composer shows
    /// the validation error live.
    fn default() -> Self {
        Self {
            question: String::new(),
            answers: vec![String::new(), String::new()],
            duration_hours: POLL_DEFAULT_DURATION_HOURS,
            allow_multiselect: false,
        }
    }
}

impl Poll {
    pub fn validate(&self) -> Result<(), String> {
        if self.question.trim().is_empty() {
            return Err("bình chọn cần một câu hỏi".into());
        }
        if self.question.chars().count() > POLL_QUESTION_LIMIT {
            return Err(format!(
                "câu hỏi bình chọn vượt giới hạn {POLL_QUESTION_LIMIT} ký tự"
            ));
        }
        if self.answers.len() < POLL_MIN_ANSWERS {
            return Err(format!("bình chọn cần ít nhất {POLL_MIN_ANSWERS} đáp án"));
        }
        if self.answers.len() > POLL_MAX_ANSWERS {
            return Err(format!("bình chọn có nhiều hơn {POLL_MAX_ANSWERS} đáp án"));
        }
        for (idx, answer) in self.answers.iter().enumerate() {
            if answer.trim().is_empty() {
                return Err(format!("đáp án bình chọn {} đang trống", idx + 1));
            }
            if answer.chars().count() > POLL_ANSWER_LIMIT {
                return Err(format!(
                    "đáp án bình chọn {} vượt giới hạn {POLL_ANSWER_LIMIT} ký tự",
                    idx + 1
                ));
            }
        }
        if !(POLL_MIN_DURATION_HOURS..=POLL_MAX_DURATION_HOURS).contains(&self.duration_hours) {
            return Err(format!(
                "thời hạn bình chọn phải là {POLL_MIN_DURATION_HOURS}–{POLL_MAX_DURATION_HOURS} giờ"
            ));
        }
        Ok(())
    }

    /// Discord payload shape for one poll.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "question": { "text": self.question },
            "answers": self.answers.iter().map(|a| {
                serde_json::json!({ "poll_media": { "text": a } })
            }).collect::<Vec<_>>(),
            "duration": self.duration_hours,
            "allow_multiselect": self.allow_multiselect,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_poll() -> Poll {
        Poll {
            question: "Pizza or pasta?".to_string(),
            answers: vec!["Pizza".to_string(), "Pasta".to_string()],
            duration_hours: 24,
            allow_multiselect: false,
        }
    }

    #[test]
    fn validate_catches_empty_overlong_and_bad_duration() {
        assert!(Poll::default().validate().is_err());
        assert!(valid_poll().validate().is_ok());
        let mut p = valid_poll();
        p.question = "q".repeat(301);
        assert!(p.validate().is_err());
        let mut p = valid_poll();
        p.answers = vec!["Only one".to_string()];
        assert!(p.validate().is_err());
        let mut p = valid_poll();
        p.answers = vec!["a".to_string(); 11];
        assert!(p.validate().is_err());
        let mut p = valid_poll();
        p.answers[1] = "a".repeat(56);
        assert!(p.validate().is_err());
        let mut p = valid_poll();
        p.duration_hours = 0;
        assert!(p.validate().is_err());
        let mut p = valid_poll();
        p.duration_hours = 169;
        assert!(p.validate().is_err());
    }

    #[test]
    fn to_json_matches_discord_poll_shape() {
        let v = valid_poll().to_json();
        assert_eq!(v["question"]["text"], "Pizza or pasta?");
        assert_eq!(v["answers"].as_array().unwrap().len(), 2);
        assert_eq!(v["answers"][0]["poll_media"]["text"], "Pizza");
        assert_eq!(v["duration"], 24);
        assert_eq!(v["allow_multiselect"], false);
    }
}
