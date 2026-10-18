use crate::types::SessionState;
use std::fs;
use std::path::Path;

pub const HARD_CEILING_RATIO: f64 = 0.35;

pub struct BudgetEvaluation {
    pub saturation_ratio: f64,
    pub must_reset_session: bool,
    pub prompt_tokens: usize,
}

/// Zero-dependency BPE token estimator calibrated for code + English (~3.6 chars/token + punctuation weight)
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let char_count = text.chars().count();
    let base_tokens = (char_count as f64 / 3.6).ceil() as usize;
    let punct_or_op = text
        .chars()
        .filter(|c| c.is_ascii_punctuation() || *c == '\n')
        .count();
    base_tokens + (punct_or_op / 3)
}

pub fn load_session(session_file: &Path) -> SessionState {
    fs::read_to_string(session_file)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_session(session_file: &Path, state: &SessionState) -> std::io::Result<()> {
    let serialized = serde_json::to_string_pretty(state)?;
    fs::write(session_file, serialized)
}

pub fn evaluate_budget(
    session: &SessionState,
    raw_prompt: &str,
    base_overhead_tokens: usize,
    max_window_tokens: usize,
) -> BudgetEvaluation {
    let prompt_tokens = estimate_tokens(raw_prompt);
    let projected_total = base_overhead_tokens + session.accumulated_tokens + prompt_tokens;
    let max_safe = max_window_tokens.max(1);
    let saturation_ratio = (projected_total as f64) / (max_safe as f64);

    BudgetEvaluation {
        saturation_ratio,
        must_reset_session: saturation_ratio > HARD_CEILING_RATIO,
        prompt_tokens,
    }
}

pub fn commit_turn_to_ledger(
    session: &mut SessionState,
    eval: &BudgetEvaluation,
    raw_prompt: &str,
    compiled_prompt: &str,
    handoff_summary: Option<String>,
) {
    let compiled_tokens = estimate_tokens(compiled_prompt);
    if eval.must_reset_session {
        session.session_id += 1;
        let handoff = handoff_summary.unwrap_or_default();
        let handoff_tokens = estimate_tokens(&handoff);
        session.accumulated_tokens = handoff_tokens + compiled_tokens;
        session.rolling_summary = handoff;
    } else {
        session.accumulated_tokens += compiled_tokens;
        let snippet: String = raw_prompt
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        if !snippet.is_empty() {
            if !session.rolling_summary.is_empty() {
                session.rolling_summary.push_str(" -> ");
            }
            session.rolling_summary.push_str(&snippet);
            // Cap rolling_summary to last ~600 chars (UTF-8 safe)
            if session.rolling_summary.len() > 600 {
                let mut start = session.rolling_summary.len() - 600;
                while start < session.rolling_summary.len()
                    && !session.rolling_summary.is_char_boundary(start)
                {
                    start += 1;
                }
                session.rolling_summary = session.rolling_summary[start..].to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_reset_strictly_above_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 35_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 35,000 history + ~100 prompt = 45,100 / 128,000 = 35.23%
        let eval = evaluate_budget(&state, &"a".repeat(360), 10_000, 128_000);
        assert!(eval.must_reset_session);
        assert!(eval.saturation_ratio > 0.35);
    }

    #[test]
    fn stays_in_session_at_or_below_35_percent() {
        let state = SessionState {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 20_000,
            rolling_summary: String::new(),
        };
        // 10,000 overhead + 20,000 history + ~10 prompt = 30,010 / 128,000 = 23.4%
        let eval = evaluate_budget(&state, "fix typo", 10_000, 128_000);
        assert!(!eval.must_reset_session);
    }
}
