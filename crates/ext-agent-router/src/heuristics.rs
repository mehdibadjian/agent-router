//! System 1 lexical / trigger matching for subagents (spec §4.2.3).
//!
//! Pure and deterministic: lowercase the prompt, tokenize it, then score each declared
//! capability. No allocations per candidate beyond the score vector, no I/O, no clock.
//! The confidence model is documented at `score_candidate`.

use crate::types::{AgentEntry, AgentRegistry};
use serde::{Deserialize, Serialize};

/// Multiplier for a trigger phrase that appears verbatim in the prompt.
const EXACT_TRIGGER_WEIGHT: f32 = 2.0;
/// Multiplier for a capability keyword hitting a prompt token.
const CAPABILITY_WEIGHT: f32 = 1.0;
/// Bonus when the entry id itself appears in the prompt (an explicit `use rust-architect`).
const ID_MATCH_BONUS: f32 = 3.0;

/// Fast-path output carried into the gate.
///
/// Every field has a serde default so a partially-populated legacy payload (the shape the
/// old dispatcher logged) still deserializes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicCandidate {
    /// Best-matched `agent` entry, if any scored above zero.
    #[serde(default)]
    pub matched_agent: Option<AgentEntry>,
    /// Runner-up agents, in descending score order — System 2 uses these to break ties.
    #[serde(default)]
    pub runner_ups: Vec<AgentEntry>,
    /// Skills whose keywords appear in the prompt, in registry order.
    #[serde(default)]
    pub matched_skills: Vec<AgentEntry>,
    /// `0.0..=1.0`. How decisively `matched_agent` beat the field.
    #[serde(default)]
    pub confidence: f32,
    /// Trigger phrases that actually matched, for traceability.
    #[serde(default)]
    pub matched_triggers: Vec<String>,
    /// Number of agents scoring above the ambiguity floor.
    #[serde(default)]
    pub candidate_count: usize,
}

impl HeuristicCandidate {
    pub fn best_agent(&self) -> Option<&AgentEntry> {
        self.matched_agent.as_ref()
    }

    pub fn skill_paths(&self) -> Vec<String> {
        self.matched_skills.iter().map(|e| e.path.clone()).collect()
    }

    pub fn skill_ids(&self) -> Vec<String> {
        self.matched_skills.iter().map(|e| e.id.clone()).collect()
    }
}

/// Split a prompt into lowercase word tokens, dropping punctuation.
fn tokenize(prompt: &str) -> Vec<String> {
    prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Raw score for one entry: id bonus + trigger phrases + capability keyword hits.
fn raw_score(entry: &AgentEntry, lowered: &str, tokens: &[String]) -> (f32, Vec<String>) {
    let mut score = 0.0f32;
    let mut hits = Vec::new();

    if !entry.id.is_empty() && lowered.contains(&entry.id.to_lowercase()) {
        score += ID_MATCH_BONUS;
        hits.push(entry.id.clone());
    }

    for trigger in &entry.triggers {
        let needle = trigger.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        if lowered.contains(&needle) {
            score += EXACT_TRIGGER_WEIGHT * needle.split_whitespace().count() as f32;
            hits.push(trigger.clone());
        }
    }

    for keyword in &entry.capabilities {
        let needle = keyword.to_lowercase();
        if tokens.iter().any(|t| *t == needle) {
            score += CAPABILITY_WEIGHT;
            hits.push(keyword.clone());
        }
    }

    (score, hits)
}

/// Score the whole registry and derive confidence from the winning margin.
pub fn score_candidate(prompt: &str, registry: &AgentRegistry) -> HeuristicCandidate {
    let lowered = prompt.to_lowercase();
    let tokens = tokenize(prompt);

    let mut agents: Vec<(f32, AgentEntry, Vec<String>)> = Vec::new();
    let mut skills: Vec<AgentEntry> = Vec::new();

    for entry in registry.values() {
        let (score, hits) = raw_score(entry, &lowered, &tokens);
        if entry.is_skill() {
            if score > 0.0 {
                skills.push(entry.clone());
            }
        } else if score > 0.0 {
            agents.push((score, entry.clone(), hits));
        }
    }

    // Descending score, then id for deterministic tie-breaking.
    agents.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.id.cmp(&b.1.id)));

    // Any keyword above the floor is a live candidate for the ambiguity count.
    let floor = agents.first().map(|(s, _, _)| *s * 0.5).unwrap_or(0.0);
    let candidate_count = agents.iter().filter(|(s, _, _)| *s >= floor.max(0.5)).count();

    let Some((best_score, best, best_hits)) = agents.first().cloned() else {
        return HeuristicCandidate {
            matched_agent: None,
            runner_ups: Vec::new(),
            matched_skills: skills,
            confidence: 0.0,
            matched_triggers: Vec::new(),
            candidate_count: 0,
        };
    };

    let second_score = agents.get(1).map(|(s, _, _)| *s).unwrap_or(0.0);
    // Winning-margin confidence: an uncontested match scores 1.0, a dead tie scores 0.0,
    // and anything in between reflects how much clearer the winner was.
    let confidence = if best_score <= 0.0 {
        0.0
    } else {
        ((best_score - second_score) / best_score).clamp(0.0, 1.0)
    };

    HeuristicCandidate {
        matched_agent: Some(best),
        runner_ups: agents.into_iter().skip(1).map(|(_, e, _)| e).collect(),
        matched_skills: skills,
        confidence,
        matched_triggers: best_hits,
        candidate_count,
    }
}

/// Trivial commands never need compilation: short shell/test invocations pass through.
pub fn is_trivial_passthrough(raw_prompt: &str) -> bool {
    let trimmed = raw_prompt.trim();
    let word_count = trimmed.split_whitespace().count();
    let command_prefixes = ["cargo ", "git ", "npm ", "pytest", "ls", "cd ", "/clear", "/help"];
    word_count <= 4 && command_prefixes.iter().any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentEntry;

    fn registry() -> AgentRegistry {
        let mut r = AgentRegistry::new();
        let agent = AgentEntry {
            id: "rust-architect".into(),
            kind: "agent".into(),
            path: ".agents/rust-architect.md".into(),
            description: "Systems architect".into(),
            triggers: vec!["rust".into(), "borrow".into(), "lifetime".into()],
            name: Some("rust-architect".into()),
            capabilities: Vec::new(),
            model_tier: None,
        };
        let skill = AgentEntry {
            id: "tdd-workflow".into(),
            kind: "skill".into(),
            path: "skills/tdd/SKILL.md".into(),
            description: "Red-Green-Refactor".into(),
            triggers: vec!["test".into(), "tdd".into()],
            name: None,
            capabilities: Vec::new(),
            model_tier: None,
        };
        let other = AgentEntry {
            id: "db-migrator".into(),
            kind: "agent".into(),
            path: ".agents/db-migrator.md".into(),
            description: "SQL migrations".into(),
            triggers: Vec::new(),
            name: Some("db-migrator".into()),
            capabilities: vec!["sql".into(), "migration".into()],
            model_tier: None,
        };
        r.insert(agent.id.clone(), agent);
        r.insert(skill.id.clone(), skill);
        r.insert(other.id.clone(), other);
        r
    }

    #[test]
    fn explicit_id_mention_is_decisive() {
        let candidate = score_candidate("activate rust-architect to fix the lifetime bug", &registry());
        assert_eq!(candidate.matched_agent.as_ref().unwrap().id, "rust-architect");
        assert!(candidate.confidence >= 0.85, "confidence {}", candidate.confidence);
        assert_eq!(candidate.candidate_count, 1);
        assert!(candidate.matched_triggers.iter().any(|t| t == "rust" || t == "lifetime"));
        // No skill keyword appears in this prompt, so the skill surface stays empty.
        assert!(candidate.skill_ids().is_empty(), "{:?}", candidate.skill_ids());

        let with_skill =
            score_candidate("activate rust-architect and write the tdd harness", &registry());
        assert_eq!(with_skill.skill_ids(), vec!["tdd-workflow".to_string()]);
    }

    #[test]
    fn no_match_scores_zero() {
        let candidate = score_candidate("reorganise the widget factory then maybe", &registry());
        assert!(candidate.matched_agent.is_none());
        assert_eq!(candidate.confidence, 0.0);
        assert_eq!(candidate.candidate_count, 0);
    }

    #[test]
    fn tied_agents_report_low_confidence_and_two_candidates() {
        let mut r = registry();
        let twin = AgentEntry {
            id: "rust-architect-2".into(),
            kind: "agent".into(),
            path: ".agents/rust-architect-2.md".into(),
            description: "Second".into(),
            triggers: vec!["rust".into(), "borrow".into(), "lifetime".into()],
            name: None,
            capabilities: Vec::new(),
            model_tier: None,
        };
        r.insert(twin.id.clone(), twin);
        let candidate = score_candidate("rust lifetime", &r);
        assert!(candidate.confidence < 0.85, "tied field must not commit: {}", candidate.confidence);
        assert!(candidate.candidate_count >= 2, "{}", candidate.candidate_count);
    }

    #[test]
    fn capability_keywords_are_matched_as_tokens() {
        let candidate = score_candidate("apply this sql change", &registry());
        assert_eq!(candidate.matched_agent.as_ref().unwrap().id, "db-migrator");
    }

    #[test]
    fn matching_is_pure_and_repeatable() {
        let r = registry();
        let a = score_candidate("fix rust borrow checker", &r);
        let b = score_candidate("fix rust borrow checker", &r);
        assert_eq!(a.confidence, b.confidence);
        assert_eq!(a.matched_agent, b.matched_agent);
    }

    #[test]
    fn trivial_commands_pass_through() {
        assert!(is_trivial_passthrough("cargo test"));
        assert!(is_trivial_passthrough("git status"));
        assert!(!is_trivial_passthrough("cargo test and then refactor the parser layer"));
        assert!(!is_trivial_passthrough("implement a trie"));
    }
}
