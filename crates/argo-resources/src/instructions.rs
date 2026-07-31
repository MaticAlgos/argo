//! Project instruction discovery.
//!
//! Every supported CLI reads project conventions from a file, but they disagree on
//! the name: Codex and OpenCode read `AGENTS.md`, Claude reads `CLAUDE.md`, Kiro
//! reads steering files. An agent switched mid-conversation would otherwise lose
//! the project's conventions simply because it looks for a different filename.
//!
//! Argo discovers all of them and folds them into the context package, so the
//! conventions survive a switch. Following OpenCode's rule, discovery walks up from
//! the workspace toward the repository root, because a nested package's
//! instructions and the repository's both apply.

use argo_core::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Filenames recognized as project instructions, in precedence order.
///
/// `AGENTS.md` first because it is the cross-vendor convention; the others are
/// vendor-specific but equally authoritative to their own CLI.
const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "OPENCODE.md",
    ".cursorrules",
    ".windsurfrules",
];

/// Largest instruction file Argo will inline.
///
/// A very large file is usually generated or a mistake, and inlining it would
/// crowd out the conversation it is meant to support.
const MAX_BYTES: usize = 32 * 1024;

/// How far up the directory tree discovery walks.
const MAX_DEPTH: usize = 8;

/// One discovered instruction file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instructions {
    /// Absolute path.
    pub path: PathBuf,
    /// Filename, for display.
    pub name: String,
    /// File contents, possibly truncated.
    pub body: String,
    /// True when the body was clipped at [`MAX_BYTES`].
    pub truncated: bool,
}

/// Discovers instruction files applying to `workspace`.
///
/// Walks upward until a repository root is reached, keeping the first file of each
/// name so a nested `AGENTS.md` wins over the repository's.
pub fn discover(workspace: &Path) -> Result<Vec<Instructions>> {
    let mut found: Vec<Instructions> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();
    let mut current = Some(workspace.to_path_buf());
    let mut depth = 0usize;

    while let Some(directory) = current {
        if depth > MAX_DEPTH {
            break;
        }

        for name in INSTRUCTION_FILES {
            if seen_names.iter().any(|seen| seen == name) {
                continue;
            }
            let candidate = directory.join(name);
            let Ok(raw) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            if raw.trim().is_empty() {
                continue;
            }
            let truncated = raw.len() > MAX_BYTES;
            let body = if truncated {
                let mut end = MAX_BYTES;
                while end > 0 && !raw.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}\n… [truncated]", &raw[..end])
            } else {
                raw
            };
            seen_names.push((*name).to_string());
            found.push(Instructions {
                path: candidate,
                name: (*name).to_string(),
                body,
                truncated,
            });
        }

        // Stop at the repository root: above it, files belong to other projects.
        if directory.join(".git").exists() {
            break;
        }
        current = directory.parent().map(Path::to_path_buf);
        depth += 1;
    }

    Ok(found)
}

/// Renders discovered instructions as a prompt section.
///
/// Bodies are inlined because a switched agent will not otherwise read a file it
/// does not know to look for, which is the entire point of collecting them.
pub fn render_prompt_section(instructions: &[Instructions]) -> String {
    if instructions.is_empty() {
        return String::new();
    }
    let mut sections = vec!["## Project instructions".to_string()];
    for entry in instructions {
        sections.push(format!(
            "### {} ({})\n{}",
            entry.name,
            entry.path.display(),
            entry.body.trim()
        ));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join(name), body).expect("write");
    }

    #[test]
    fn discovers_the_cross_vendor_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "AGENTS.md", "Use tabs, not spaces.");
        let found = discover(dir.path()).expect("discover");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "AGENTS.md");
        assert!(found[0].body.contains("Use tabs"));
        assert!(!found[0].truncated);
    }

    #[test]
    fn collects_every_vendor_dialect_so_a_switch_keeps_conventions() {
        // Codex reads AGENTS.md and Claude reads CLAUDE.md; switching between them
        // must not drop either set of conventions.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "AGENTS.md", "shared conventions");
        write(dir.path(), "CLAUDE.md", "claude specific");
        write(dir.path(), ".cursorrules", "cursor specific");
        let names: Vec<String> = discover(dir.path())
            .expect("discover")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert!(names.contains(&"AGENTS.md".to_string()));
        assert!(names.contains(&"CLAUDE.md".to_string()));
        assert!(names.contains(&".cursorrules".to_string()));
    }

    #[test]
    fn walks_upward_and_prefers_the_nearest_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let nested = root.join("packages").join("api");
        write(root, "AGENTS.md", "repository wide");
        write(&nested, "AGENTS.md", "package specific");
        std::fs::create_dir_all(root.join(".git")).expect("git marker");

        let found = discover(&nested).expect("discover");
        let agents: Vec<&Instructions> = found.iter().filter(|i| i.name == "AGENTS.md").collect();
        assert_eq!(agents.len(), 1, "only the nearest file wins a name");
        assert!(agents[0].body.contains("package specific"));
    }

    #[test]
    fn a_repository_root_stops_the_upward_walk() {
        // Files above the repository belong to unrelated projects.
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path();
        let repo = outer.join("repo");
        write(outer, "CLAUDE.md", "someone else's project");
        std::fs::create_dir_all(repo.join(".git")).expect("git marker");
        write(&repo, "AGENTS.md", "this project");

        let names: Vec<String> = discover(&repo)
            .expect("discover")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert!(names.contains(&"AGENTS.md".to_string()));
        assert!(
            !names.contains(&"CLAUDE.md".to_string()),
            "must not climb past the repository root"
        );
    }

    #[test]
    fn empty_files_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "AGENTS.md", "   \n\n");
        assert!(discover(dir.path()).expect("discover").is_empty());
    }

    #[test]
    fn oversized_files_are_truncated_on_a_char_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Multi-byte content would panic on a naive byte slice.
        write(dir.path(), "AGENTS.md", &"é".repeat(MAX_BYTES));
        let found = discover(dir.path()).expect("discover");
        assert!(found[0].truncated);
        assert!(found[0].body.ends_with("… [truncated]"));
        assert!(found[0].body.len() <= MAX_BYTES + 32);
    }

    #[test]
    fn a_missing_workspace_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover(&dir.path().join("nope"))
            .expect("discover")
            .is_empty());
    }

    #[test]
    fn the_prompt_section_inlines_bodies_with_their_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "AGENTS.md", "Never commit secrets.");
        let section = render_prompt_section(&discover(dir.path()).expect("discover"));
        assert!(section.contains("## Project instructions"));
        assert!(section.contains("### AGENTS.md"));
        assert!(section.contains("Never commit secrets."));
    }

    #[test]
    fn no_instructions_produces_no_section() {
        assert_eq!(render_prompt_section(&[]), "");
    }
}
