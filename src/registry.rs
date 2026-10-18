use crate::types::{AgentEntry, RegistryCache};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;
use xxhash_rust::xxh3::xxh3_64;

const SEARCH_DIRS: &[(&str, &str)] = &[
    (".agents", "agent"),
    (".claude/agents", "agent"),
    (".github/agents", "agent"),
    ("skills", "skill"),
    (".claude/skills", "skill"),
    (".github/skills", "skill"),
    (".github/prompts", "skill"),
    (".github/instructions", "skill"),
];

pub fn load_or_build_registry(
    repo_root: &Path,
    cache_file: &Path,
) -> BTreeMap<String, AgentEntry> {
    let mut fingerprint = String::new();
    let mut discovered_files: Vec<(PathBuf, &'static str)> = Vec::new();

    for (rel_dir, kind) in SEARCH_DIRS {
        let full_dir = repo_root.join(rel_dir);
        if !full_dir.exists() {
            continue;
        }
        let mut paths: Vec<PathBuf> = WalkDir::new(&full_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        paths.sort();

        for path in paths {
            if let Ok(meta) = fs::metadata(&path) {
                let nanos = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                fingerprint.push_str(&format!("{}:{}|", path.display(), nanos));
                discovered_files.push((path, *kind));
            }
        }
    }

    let current_hash = xxh3_64(fingerprint.as_bytes());

    if let Ok(raw) = fs::read_to_string(cache_file) {
        if let Ok(cached) = serde_json::from_str::<RegistryCache>(&raw) {
            if cached.hash == current_hash {
                return cached.entries;
            }
        }
    }

    let mut entries = BTreeMap::new();
    for (file_path, kind) in discovered_files {
        if let Ok(content) = fs::read_to_string(&file_path) {
            let entry = parse_manifest(repo_root, &file_path, kind, &content);
            entries.insert(entry.id.clone(), entry);
        }
    }

    let cache = RegistryCache {
        hash: current_hash,
        entries: entries.clone(),
    };
    let _ = fs::write(cache_file, serde_json::to_string_pretty(&cache).unwrap_or_default());
    entries
}

/// Zero-dependency parser for YAML frontmatter (`--- ... ---`) with Markdown header fallback
pub fn parse_manifest(
    repo_root: &Path,
    file_path: &Path,
    kind: &str,
    content: &str,
) -> AgentEntry {
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
                    "triggers" => {
                        let cleaned = val.trim_start_matches('[').trim_end_matches(']');
                        triggers = cleaned
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
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

    AgentEntry {
        id,
        kind: kind.to_string(),
        path: rel_path,
        description,
        triggers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frontmatter_accurately() {
        let md = "---\nname: sec-auditor\ndescription: Audits Rust unsafe blocks\ntriggers: [security, unsafe, audit]\n---\n# Body";
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/.agents/sec.md"), "agent", md);
        assert_eq!(entry.id, "sec-auditor");
        assert_eq!(entry.description, "Audits Rust unsafe blocks");
        assert_eq!(entry.triggers, vec!["security", "unsafe", "audit"]);
    }

    #[test]
    fn strips_agent_and_instructions_double_extension_when_name_omitted() {
        let md = "---\ndescription: Custom VS Code agent without explicit name\ntriggers: [rust]\n---\n# Body";
        let entry = parse_manifest(
            Path::new("/repo"),
            Path::new("/repo/.github/agents/rust-architect.agent.md"),
            "agent",
            md,
        );
        assert_eq!(entry.id, "rust-architect");

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
    }

    #[test]
    fn truncates_long_multibyte_description_without_panicking() {
        // 2-byte chars make byte index 240 land mid-char; must snap back to a boundary.
        let md = format!("# Title\n{}", "é".repeat(300));
        let entry = parse_manifest(Path::new("/repo"), Path::new("/repo/no-front.md"), "agent", &md);
        assert!(entry.description.len() <= 240);
        assert!(entry.description.is_char_boundary(entry.description.len()));
    }
}
