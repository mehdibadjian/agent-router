//! Session continuity & handoff artifacts (spec §3.3.2).
//!
//! A halt is only useful if the next session can pick up where this one stopped, so
//! the ledger, the pathway taken, and a summary travel together as a typed envelope
//! that a downstream harness can deserialize or a human can read as Markdown.

use crate::domain::{CircuitTrip, EscalationReason};
use crate::s2::xml_compiler::XmlEnvelopeBuilder;
use serde::{Deserialize, Serialize};

/// Which cognitive pathway produced the last decision. Serialized as a stable snake-case
/// slug so persisted ledgers stay readable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathwayKind {
    #[serde(rename = "system1_fast")]
    System1Fast,
    #[serde(rename = "system2_deliberative")]
    System2Deliberative,
    #[serde(rename = "circuit_breaker_halt")]
    CircuitBreakerHalt,
}

impl Default for PathwayKind {
    fn default() -> Self {
        PathwayKind::System1Fast
    }
}

impl PathwayKind {
    pub fn slug(self) -> &'static str {
        match self {
            PathwayKind::System1Fast => "system1_fast",
            PathwayKind::System2Deliberative => "system2_deliberative",
            PathwayKind::CircuitBreakerHalt => "circuit_breaker_halt",
        }
    }

    pub fn legacy_action(self) -> &'static str {
        match self {
            PathwayKind::CircuitBreakerHalt => "START_NEW_SESSION",
            _ => "CONTINUE_SESSION",
        }
    }
}

/// The continuity record a domain keeps between turns (Extension #1's `SessionState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLedger {
    pub session_id: u64,
    #[serde(default)]
    pub host_session_id: Option<String>,
    #[serde(default)]
    pub accumulated_tokens: usize,
    #[serde(default)]
    pub rolling_summary: String,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub last_pathway: PathwayKind,
}

impl Default for SessionLedger {
    fn default() -> Self {
        Self {
            session_id: 1,
            host_session_id: None,
            accumulated_tokens: 0,
            rolling_summary: String::new(),
            turn_count: 0,
            last_pathway: PathwayKind::default(),
        }
    }
}

/// Generic session envelope (spec §3.3.2): identity + budget + pathway + summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvelope<T> {
    pub session_id: u64,
    pub turn_count: u64,
    pub cumulative_tokens: usize,
    pub last_pathway: PathwayKind,
    pub handoff_summary: String,
    pub domain_state: T,
}

impl<T> SessionEnvelope<T> {
    pub fn new(ledger: &SessionLedger, domain_state: T) -> Self {
        Self {
            session_id: ledger.session_id,
            turn_count: ledger.turn_count,
            cumulative_tokens: ledger.accumulated_tokens,
            last_pathway: ledger.last_pathway,
            handoff_summary: ledger.rolling_summary.clone(),
            domain_state,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> SessionEnvelope<U> {
        SessionEnvelope {
            session_id: self.session_id,
            turn_count: self.turn_count,
            cumulative_tokens: self.cumulative_tokens,
            last_pathway: self.last_pathway,
            handoff_summary: self.handoff_summary,
            domain_state: f(self.domain_state),
        }
    }
}

/// Why continuity was handed off, so the next session can render it faithfully.
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffCause {
    CircuitTripped(CircuitTrip),
    Escalated(EscalationReason),
}

/// Deterministic handoff payload for downstream execution harnesses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffArtifact {
    pub session_id: u64,
    pub turn_count: u64,
    pub cumulative_tokens: usize,
    pub last_pathway: PathwayKind,
    pub saturation_ratio: f32,
    pub summary: String,
}

/// Render the Markdown/XML artifact a fresh session reads on its first turn.
pub fn render_handoff_artifact(
    ledger: &SessionLedger,
    cause: &HandoffCause,
    saturation_ratio: f32,
    max_saturation_ratio: f32,
) -> String {
    let mut b = XmlEnvelopeBuilder::new();

    match cause {
        HandoffCause::CircuitTripped(trip) => b.open(
            "circuit_breaker_tripped",
            &[
                ("max", &format!("{:.2}", max_saturation_ratio)),
                ("trip", &trip.to_string()),
                ("saturation", &format!("{:.3}", saturation_ratio)),
            ],
        ),
        HandoffCause::Escalated(reason) => b.open(
            "escalation_handoff",
            &[
                ("reason", &reason.to_string()),
                ("saturation", &format!("{:.3}", saturation_ratio)),
            ],
        ),
    };

    b.open(
        "session_envelope",
        &[
            ("session_id", &ledger.session_id.to_string()),
            ("turn_count", &ledger.turn_count.to_string()),
            ("cumulative_tokens", &ledger.accumulated_tokens.to_string()),
            ("last_pathway", ledger.last_pathway.slug()),
        ],
    );
    b.text_tag("handoff_summary", &[], &summary_or_placeholder(ledger));
    b.close("session_envelope");
    b.close(match cause {
        HandoffCause::CircuitTripped(_) => "circuit_breaker_tripped",
        HandoffCause::Escalated(_) => "escalation_handoff",
    });
    b.finish()
}

fn summary_or_placeholder(ledger: &SessionLedger) -> String {
    if ledger.rolling_summary.trim().is_empty() {
        "None recorded.".to_string()
    } else {
        ledger.rolling_summary.clone()
    }
}

/// Load a persisted ledger, defaulting when the file is missing or unreadable so a
/// fresh workspace still routes.
pub fn load_ledger(path: &std::path::Path) -> SessionLedger {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_ledger(path: &std::path::Path, ledger: &SessionLedger) -> std::io::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(ledger)?)
}

/// Commit one turn to the token ledger, rolling the session forward when the ceiling was
/// breached and keeping the rolling summary bounded to the last ~600 characters.
pub fn commit_turn(
    ledger: &mut SessionLedger,
    ceiling_breached: bool,
    pathway: PathwayKind,
    raw_prompt: &str,
    compiled_prompt: &str,
    handoff_summary: Option<String>,
    token_estimate: fn(&str) -> usize,
) {
    let compiled_tokens = token_estimate(compiled_prompt);
    ledger.turn_count += 1;
    ledger.last_pathway = pathway;

    if ceiling_breached {
        ledger.session_id += 1;
        let handoff = handoff_summary.unwrap_or_default();
        ledger.accumulated_tokens = token_estimate(&handoff) + compiled_tokens;
        ledger.rolling_summary = handoff;
        return;
    }

    ledger.accumulated_tokens += compiled_tokens;
    let snippet: String = raw_prompt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    if snippet.is_empty() {
        return;
    }
    if !ledger.rolling_summary.is_empty() {
        ledger.rolling_summary.push_str(" -> ");
    }
    ledger.rolling_summary.push_str(&snippet);
    if ledger.rolling_summary.len() > 600 {
        let mut start = ledger.rolling_summary.len() - 600;
        while start < ledger.rolling_summary.len() && !ledger.rolling_summary.is_char_boundary(start) {
            start += 1;
        }
        ledger.rolling_summary = ledger.rolling_summary[start..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> SessionLedger {
        SessionLedger {
            session_id: 3,
            host_session_id: Some("host-1".into()),
            accumulated_tokens: 75_000,
            rolling_summary: "design ast node structs".into(),
            turn_count: 7,
            last_pathway: PathwayKind::System2Deliberative,
        }
    }

    #[test]
    fn artifact_preserves_state_and_ceiling() {
        let doc = render_handoff_artifact(
            &ledger(),
            &HandoffCause::CircuitTripped(CircuitTrip::SaturationExceeded {
                current_ratio: 0.375,
                max_ratio: 0.35,
            }),
            0.375,
            0.35,
        );
        assert!(doc.starts_with("<circuit_breaker_tripped max=\"0.35\""), "{doc}");
        assert!(doc.contains("<session_envelope session_id=\"3\" turn_count=\"7\""), "{doc}");
        assert!(doc.contains("cumulative_tokens=\"75000\""), "{doc}");
        assert!(doc.contains("last_pathway=\"system2_deliberative\""), "{doc}");
        assert!(doc.contains("<handoff_summary>design ast node structs</handoff_summary>"), "{doc}");
        assert!(doc.ends_with("</circuit_breaker_tripped>\n"), "{doc}");
    }

    #[test]
    fn empty_summary_gets_a_placeholder() {
        let mut l = ledger();
        l.rolling_summary = String::new();
        let doc = render_handoff_artifact(
            &l,
            &HandoffCause::Escalated(EscalationReason::ExplicitSystem2Request),
            0.31,
            0.35,
        );
        assert!(doc.starts_with("<escalation_handoff reason=\"explicit-system2-request\""), "{doc}");
        assert!(doc.contains("None recorded."), "{doc}");
    }

    #[test]
    fn envelope_maps_domain_state_and_legacy_action() {
        let env = SessionEnvelope::new(&ledger(), 42usize);
        assert_eq!(env.session_id, 3);
        assert_eq!(env.cumulative_tokens, 75_000);
        assert_eq!(env.handoff_summary, "design ast node structs");
        assert_eq!(env.map(|n| n.to_string()).domain_state, "42");
        assert_eq!(PathwayKind::CircuitBreakerHalt.legacy_action(), "START_NEW_SESSION");
        assert_eq!(PathwayKind::System1Fast.legacy_action(), "CONTINUE_SESSION");
    }

    #[test]
    fn ledger_json_is_backward_compatible() {
        let json = serde_json::to_string(&ledger()).unwrap();
        assert!(json.contains("\"host_session_id\":\"host-1\""), "{json}");
        let round: SessionLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(round, ledger());
        // A legacy ledger without the v2 fields must still deserialize.
        let legacy = r#"{"session_id":2,"accumulated_tokens":5,"rolling_summary":"x"}"#;
        let parsed: SessionLedger = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.turn_count, 0);
        assert_eq!(parsed.last_pathway, PathwayKind::System1Fast);
    }
}
