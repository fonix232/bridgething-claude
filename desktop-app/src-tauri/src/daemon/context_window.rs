// Context window sizes per model. Mirrors daemon/src/context-window.js.
//
// The transcript gives what a turn sent, which is the occupancy. It does not
// carry the ceiling, and nothing else in the session data does either — so the
// denominator has to come from a table. Claude Code knows the real number (it
// hands `context_window_size` to statusLine commands), and if this daemon ever
// grows a statusLine hook, prefer that over this file.
//
// An unknown model returns None, and the device hides the meter rather than
// drawing a bar against a guess.

use regex::Regex;
use std::sync::OnceLock;

const MILLION: u64 = 1_000_000;

const PATTERNS: &[(&str, u64)] = &[
    (r"opus-5|fable-5|mythos|sonnet-5", MILLION),
    (r"opus-4|sonnet-4-6|sonnet-4\.6", MILLION),
    (r"haiku", 200_000),
];

static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();

fn compiled() -> &'static [Regex] {
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|(pattern, _)| Regex::new(pattern).expect("static context-window regex"))
            .collect()
    })
}

pub fn context_window_for(model: &str) -> Option<u64> {
    if model.is_empty() {
        return None;
    }
    compiled()
        .iter()
        .zip(PATTERNS)
        .find(|(re, _)| re.is_match(model))
        .map(|(_, (_, size))| *size)
}

/// 0..1, or None when either half of the fraction is unknown.
pub fn context_fraction(model: &str, tokens: u64) -> Option<f64> {
    let size = context_window_for(model)?;
    if tokens == 0 {
        return None;
    }
    Some((tokens as f64 / size as f64).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models() {
        assert_eq!(context_window_for("claude-opus-5-20260101"), Some(MILLION));
        assert_eq!(context_window_for("claude-haiku-4"), Some(200_000));
        assert_eq!(context_window_for("unknown-model"), None);
        assert_eq!(context_window_for(""), None);
    }

    #[test]
    fn fraction_needs_both_halves() {
        assert_eq!(context_fraction("claude-haiku-4", 0), None);
        assert_eq!(context_fraction("unknown", 100), None);
        assert_eq!(context_fraction("claude-haiku-4", 400_000), Some(1.0));
    }
}
