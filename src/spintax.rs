//! Spintax + message variation — the "Smart Templates" feature from
//! Discord-BLUE, ported to Rust.
//!
//! `{a|b|c}` picks one option at random. A `{...}` group only counts as
//! spintax when it contains `|` and no nested braces: `{{username}}`
//! template vars and single `{time}`-style tokens pass through untouched
//! (the same rule as BLUE's `process_spintax` regex). An unclosed `{`
//! is copied verbatim, never dropped.
//!
//! Variation (`vary_message`, BLUE's `randomize_content`) randomly
//! inserts an emoji, doubles a space, or flips the case of one word —
//! each with a small probability, so most messages stay natural.
//! BLUE's abbreviation mutation is deliberately NOT ported: it is
//! Vietnamese-slang specific and would mangle ordinary words.
//!
//! Applied in the send path AFTER template vars are resolved and
//! BEFORE sending, only when Smart Templates is on.

use rand::Rng;

/// Small built-in emoji pool for message variation.
const EMOJIS: &[&str] = &[
    "😊", "✨", "👍", "😄", "🔥", "💡", "🎉", "😀", "🙂", "👀", "💬", "🌟",
];

/// Pick one option from each `{a|b|c}` group. Groups without `|`, groups
/// with nested braces, and unclosed braces are left untouched.
pub fn apply_spintax(input: &str, rng: &mut impl Rng) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find the closing brace of this candidate group.
            if let Some(rel_end) = input[i + 1..].find('}') {
                let content = &input[i + 1..i + 1 + rel_end];
                if content.contains('|') && !content.contains('{') && !content.contains('}') {
                    let options: Vec<&str> = content.split('|').collect();
                    let pick = options[rng.random_range(0..options.len())];
                    out.push_str(pick.trim());
                    i = i + 1 + rel_end + 1;
                    continue;
                }
            }
            // Not a spintax group (or unclosed): copy the brace literally
            // and keep scanning, so `{{username}}` survives intact.
            out.push('{');
            i += 1;
        } else {
            // Copy one UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            let end = (i + ch_len).min(bytes.len());
            out.push_str(&input[i..end]);
            i = end;
        }
    }
    out
}

/// Length in bytes of the UTF-8 character starting with this lead byte.
fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Randomly vary a message so repeated sends do not look identical.
/// Each mutation is independent with a small probability; with the
/// default rolls most messages come out unchanged. Deterministic for a
/// seeded RNG, so tests can pin the output.
pub fn vary_message(input: &str, rng: &mut impl Rng) -> String {
    let mut out = input.to_string();
    // 1. Insert a random emoji after a random word (~15%).
    if rng.random_bool(0.15) && !out.trim().is_empty() {
        let emoji = EMOJIS[rng.random_range(0..EMOJIS.len())];
        let words: Vec<&str> = out.split(' ').collect();
        if words.len() > 1 {
            let at = rng.random_range(0..words.len());
            let mut rebuilt = String::new();
            for (idx, w) in words.iter().enumerate() {
                if idx > 0 {
                    rebuilt.push(' ');
                }
                rebuilt.push_str(w);
                if idx == at {
                    rebuilt.push(' ');
                    rebuilt.push_str(emoji);
                }
            }
            out = rebuilt;
        } else {
            out.push(' ');
            out.push_str(emoji);
        }
    }
    // 2. Double one random space (~15%).
    if rng.random_bool(0.15) {
        let spaces: Vec<usize> = out
            .char_indices()
            .filter(|(_, c)| *c == ' ')
            .map(|(i, _)| i)
            .collect();
        if !spaces.is_empty() {
            let at = spaces[rng.random_range(0..spaces.len())];
            out.insert(at, ' ');
        }
    }
    // 3. Flip the case of one random word's first letter (~15%).
    if rng.random_bool(0.15) {
        let words: Vec<&str> = out.split(' ').collect();
        let candidates: Vec<usize> = words
            .iter()
            .enumerate()
            .filter(|(_, w)| w.chars().any(|c| c.is_alphabetic()))
            .map(|(i, _)| i)
            .collect();
        if !candidates.is_empty() {
            let at = candidates[rng.random_range(0..candidates.len())];
            let mut rebuilt = String::new();
            for (idx, w) in words.iter().enumerate() {
                if idx > 0 {
                    rebuilt.push(' ');
                }
                if idx == at {
                    let mut chars = w.chars();
                    if let Some(first) = chars.next() {
                        if first.is_lowercase() {
                            rebuilt.extend(first.to_uppercase());
                        } else {
                            rebuilt.extend(first.to_lowercase());
                        }
                        rebuilt.push_str(chars.as_str());
                    }
                } else {
                    rebuilt.push_str(w);
                }
            }
            out = rebuilt;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn spintax_picks_exactly_one_option() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let out = apply_spintax("{chào|hello|hi} bạn", &mut rng);
        assert!(out == "chào bạn" || out == "hello bạn" || out == "hi bạn");
        assert!(!out.contains('{'));
    }

    #[test]
    fn spintax_leaves_single_tokens_and_template_vars_untouched() {
        let mut rng = ChaCha8Rng::seed_from_u64(2);
        // No `|` → untouched.
        assert_eq!(
            apply_spintax("hẹn lúc {time} nhé", &mut rng),
            "hẹn lúc {time} nhé"
        );
        // Double-brace template vars → untouched (no nested-brace groups).
        assert_eq!(
            apply_spintax("chào {{username}} ở {{server}}", &mut rng),
            "chào {{username}} ở {{server}}"
        );
        // Unclosed brace → copied verbatim.
        assert_eq!(apply_spintax("mở ngoặc {abc", &mut rng), "mở ngoặc {abc");
        // Empty options keep the group shape out but never panic.
        let out = apply_spintax("{|}", &mut rng);
        assert_eq!(out, "");
    }

    #[test]
    fn spintax_handles_multiple_groups_and_unicode() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let out = apply_spintax("{a|b} và {x|y} nhé 😊", &mut rng);
        assert!(out.starts_with('a') || out.starts_with('b'));
        assert!(out.contains(" và "));
        assert!(out.ends_with("nhé 😊"));
        assert!(!out.contains('{'));
    }

    #[test]
    fn variation_is_deterministic_with_seeded_rng() {
        let mut a = ChaCha8Rng::seed_from_u64(99);
        let mut b = ChaCha8Rng::seed_from_u64(99);
        let msg = "chào mọi người hôm nay vui quá";
        assert_eq!(vary_message(msg, &mut a), vary_message(msg, &mut b));
    }

    #[test]
    fn variation_never_empties_or_explodes_the_message() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        for _ in 0..100 {
            let out = vary_message("hello world this is a test", &mut rng);
            assert!(!out.trim().is_empty());
            // At most one emoji + one doubled space + one case flip.
            assert!(out.chars().count() <= "hello world this is a test".chars().count() + 3);
        }
        // Empty input stays empty.
        assert_eq!(vary_message("", &mut rng), "");
    }
}
