//! Capability discovery for Extension #1 (spec §4.2.1), built on `s1s2_core::s1::cache`.
//!
//! Parsing is unchanged from the legacy router: zero-dependency YAML frontmatter with a
//! Markdown-header fallback, plus the `.agent.md` / `.prompt.md` / `.instructions.md`
//! double-extension stripping that existing repositories rely on for their ids.

use crate::types::{AgentEntry, AgentRegistry};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Parse one manifest into an `AgentEntry`.
///
/// `kind` comes from the directory that declared the file, never from the file itself.
pub fn parse_manifest(repo_root: &Path, file_path: &Path, kind: &str, content: &str) -> AgentEntry {
    let rel_path = file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");

    let default_id = if file_path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        file_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-skill")
            .to_string()
    } else {
        let raw_stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-agent");
        raw_stem
            .strip_suffix(".agent")
            .or_else(|| raw_stem.strip_suffix(".instructions"))
            .or_else(|| raw_stem.strip_suffix(".prompt"))
            .unwrap_or(raw_stem)
            .to_string()
    };

    let mut id = default_id;
    let mut description = String::new();
    let mut triggers = Vec::new();
    let mut capabilities = Vec::new();
    let mut model_tier: Option<String> = None;

    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) == Some("---") {
        for line in lines.by_ref() {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "name" | "id" if !val.is_empty() => id = val.to_string(),
                    "description" if !val.is_empty() => description = val.to_string(),
                    "triggers" => triggers = parse_inline_list(val),
                    "capabilities" => capabilities = parse_inline_list(val),
                    "model_tier" | "model" if !val.is_empty() => model_tier = Some(val.to_string()),
                    _ => {}
                }
            }
        }
    }

    if description.is_empty() {
        description = content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("---"))
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        if description.len() > 240 {
            let mut end = 240;
            while end > 0 && !description.is_char_boundary(end) {
                end -= 1;
            }
            description.truncate(end);
        }
    }

    let name = if file_path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        None
    } else {
        Some(id.clone())
    };

    AgentEntry {
        id,
        kind: kind.to_string(),
        path: rel_path,
        description,
        triggers,
        name,
        capabilities,
        model_tier,
    }
}

fn parse_inline_list(val: &str) -> Vec<String> {
    let cleaned = val.trim_start_matches('[').trim_end_matches(']');
    cleaned
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Walk the declared directories and build the capability map.
pub fn build_registry(repo_root: &Path, search_dirs: &[(String, String)]) -> Result<AgentRegistry, String> {
    let mut entries = AgentRegistry::new();
    for file_path in discover_files(repo_root, search_dirs) {
        let Ok(content) = fs::read_to_string(&file_path) else { continue };
        let Some(kind) = kind_for(&file_path, repo_root, search_dirs) else { continue };
        let entry = parse_manifest(repo_root, &file_path, kind, &content);
        entries.insert(entry.id.clone(), entry);
    }
    Ok(entries)
}

/// Every Markdown manifest under the declared directories, in stable sorted order.
pub fn discover_files(repo_root: &Path, search_dirs: &[(String, String)]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for (dir, _) in search_dirs {
        let full_dir = repo_root.join(dir);
        if !full_dir.exists() {
            continue;
        }
        files.extend(
            WalkDir::new(&full_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md")),
        );
    }
    files.sort();
    files.dedup();
    files
}

/// Which declared directory owns this file — the innermost match wins, so `skills/tdd`
/// does not shadow `.claude/skills`.
fn kind_for<'a>(
    file_path: &Path,
    repo_root: &Path,
    search_dirs: &'a [(String, String)],
) -> Option<&'a str> {
    let absolute = normalize(file_path.to_path_buf());
    let mut best: Option<(&str, usize)> = None;
    for (dir, kind) in search_dirs {
        let root = normalize(repo_root.join(dir));
        if absolute.starts_with(&root) {
            let depth = root.matches('/').count();
            if best.map(|(_, d)| depth >= d).unwrap_or(true) {
                best = Some((kind.as_str(), depth));
            }
        }
    }
    best.map(|(kind, _)| kind)
}

fn normalize(path: PathBuf) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frontmatter_accurately() {
        let md = "---\nname: sec-auditor\ndescription: Audits Rust unsafe blocks\ntriggers: [security, unsafe, audit]\n---\n# Body";
        let entry = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.agents/sec.md"),
            "agent",
            md,
        );
        assert_eq!(entry.id, "sec-auditor");
        assert_eq!(entry.description, "Audits Rust unsafe blocks");
        assert_eq!(entry.triggers, vec!["security", "unsafe", "audit"]);
        assert!(entry.is_agent());
    }

    #[test]
    fn reads_capabilities_and_model_tier() {
        let md = "---\nname: dba\ndescription: Database work\ncapabilities: [sql, migrations]\nmodel_tier: sonnet\n---\n";
        let entry = parse_manifest(Path::new("/r"), Path::new("/r/.agents/dba.md"), "agent", md);
        assert_eq!(entry.capabilities, vec!["sql", "migrations"]);
        assert_eq!(entry.model_tier.as_deref(), Some("sonnet"));
        // Capability keywords widen the System 1 match surface.
        assert!(entry.keyword_surface().contains(&"sql".to_string()));
    }

    #[test]
    fn strips_double_extensions_when_name_omitted() {
        let md = "---\ndescription: Custom agent without explicit name\ntriggers: [rust]\n---\n# Body";
        let agent = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/agents/rust-architect.agent.md"),
            "agent",
            md,
        );
        assert_eq!(agent.id, "rust-architect");

        let instr = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/instructions/coding-style.instructions.md"),
            "skill",
            md,
        );
        assert_eq!(instr.id, "coding-style");

        let prompt = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/prompts/refactor.prompt.md"),
            "skill",
            md,
        );
        assert_eq!(prompt.id, "refactor");

        let skill = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/skills/tdd/SKILL.md"),
            "skill",
            md,
        );
        assert_eq!(skill.id, "tdd");
        assert_eq!(skill.name, None, "SKILL.md keeps the legacy no-alias shape");
    }

    #[test]
    fn truncates_long_multibyte_description_without_panicking() {
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }

    #[test]
    fn kind_is_taken_from_the_owning_directory() {
        let repo = std::env::temp_dir().join(format!("ext-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join(".agents")).unwrap();
        fs::create_dir_all(repo.join("skills/tdd")).unwrap();
        let agent_file = repo.join(".agents/arch.md");
        let skill_file = repo.join("skills/tdd/SKILL.md");
        fs::write(&agent_file, "---\nname: arch\n---\n").unwrap();
        fs::write(&skill_file, "---\nname: tdd-workflow\n---\n").unwrap();

        let dirs = vec![
            (".agents".to_string(), "agent".to_string()),
            ("skills".to_string(), "skill".to_string()),
        ];
        let registry = build_registry(&repo, &dirs).unwrap();
        assert_eq!(registry.get("arch").unwrap().kind, "agent");
        assert_eq!(registry.get("tdd-workflow").unwrap().kind, "skill");
        assert_eq!(registry.get("tdd-workflow").unwrap().path, "skills/tdd/SKILL.md");
        let _ = fs::remove_dir_all(&repo);
    }
}
