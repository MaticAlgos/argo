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

use argo_core::error::{ArgoError, Result};
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

/// Project-local instructions managed by the user through `/instructions`.
pub const ARGO_INSTRUCTIONS_FILE: &str = ".argo-instructions.md";

/// Presence of this ignored marker opts the project into automatic capture and injection.
const ENABLED_MARKER: &str = "instructions-enabled";

const INITIAL_BODY: &str = "# Argo project instructions\n\n<!-- Enabled with /instructions enable. Argo appends only prompts that clearly look like durable project instructions. Edit this file freely with /instructions edit. -->\n";

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

    // Argo-managed instructions are deliberately project-local and opt-in. A
    // retained file is ignored after `/instructions disable`, even though the
    // vendor instruction files below remain authoritative as before.
    if is_enabled(workspace) {
        let candidate = instructions_path(workspace);
        if let Ok(raw) = std::fs::read_to_string(&candidate) {
            if has_meaningful_instructions(&raw) {
                let (body, truncated) = bounded_body(raw);
                seen_names.push(ARGO_INSTRUCTIONS_FILE.to_string());
                found.push(Instructions {
                    path: candidate,
                    name: ARGO_INSTRUCTIONS_FILE.to_string(),
                    body,
                    truncated,
                });
            }
        }
    }

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
            let (body, truncated) = bounded_body(raw);
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

fn bounded_body(raw: String) -> (String, bool) {
    let truncated = raw.len() > MAX_BYTES;
    if !truncated {
        return (raw, false);
    }
    let mut end = MAX_BYTES;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n… [truncated]", &raw[..end]), true)
}

fn has_meaningful_instructions(raw: &str) -> bool {
    let mut in_comment = false;
    raw.lines().any(|line| {
        let mut line = line.trim();
        if in_comment {
            if let Some((_, after)) = line.split_once("-->") {
                in_comment = false;
                line = after.trim();
            } else {
                return false;
            }
        }
        if let Some((before, after)) = line.split_once("<!--") {
            if !before.trim().is_empty() && !before.trim().starts_with('#') {
                return true;
            }
            in_comment = !after.contains("-->");
            return false;
        }
        !line.is_empty() && !line.starts_with('#')
    })
}

/// Absolute path edited by `/instructions edit`.
pub fn instructions_path(workspace: &Path) -> PathBuf {
    workspace.join(ARGO_INSTRUCTIONS_FILE)
}

fn marker_path(workspace: &Path) -> PathBuf {
    workspace
        .join(argo_core::ARGO_WORKSPACE_DIR)
        .join(ENABLED_MARKER)
}

/// Whether automatic project instructions are active for this workspace.
pub fn is_enabled(workspace: &Path) -> bool {
    marker_path(workspace).is_file()
}

/// Creates the editable file when it does not exist.
pub fn ensure_file(workspace: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(workspace)?;
    let path = instructions_path(workspace);
    if !path.exists() {
        std::fs::write(&path, INITIAL_BODY)?;
    }
    Ok(path)
}

/// Enables or disables automatic capture and prompt injection without deleting the file.
pub fn set_enabled(workspace: &Path, enabled: bool) -> Result<PathBuf> {
    let path = ensure_file(workspace)?;
    let marker = marker_path(workspace);
    if enabled {
        let argo_dir = marker.parent().expect("marker always has a parent");
        std::fs::create_dir_all(argo_dir)?;
        let ignore = argo_dir.join(".gitignore");
        let mut ignore_body = std::fs::read_to_string(&ignore).unwrap_or_default();
        let covers_marker = ignore_body.lines().any(|line| {
            let line = line.trim();
            line == "*" || line == ENABLED_MARKER || line.strip_prefix('/') == Some(ENABLED_MARKER)
        });
        if !covers_marker {
            if !ignore_body.is_empty() && !ignore_body.ends_with('\n') {
                ignore_body.push('\n');
            }
            ignore_body.push_str("# Argo project-local runtime state.\n");
            ignore_body.push_str(ENABLED_MARKER);
            ignore_body.push('\n');
            std::fs::write(ignore, ignore_body)?;
        }
        std::fs::write(marker, "enabled\n")?;
    } else {
        match std::fs::remove_file(marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ArgoError::Io(format!("disable instructions: {error}"))),
        }
    }
    Ok(path)
}

/// Appends durable directives found in a user prompt when the project opted in.
///
/// This intentionally avoids turning every task into permanent policy. The
/// capture is deterministic and limited to language that explicitly signals a
/// lasting preference; `/instructions edit` remains authoritative.
pub fn capture_user_directives(workspace: &Path, prompt: &str) -> Result<Vec<String>> {
    if !is_enabled(workspace) {
        return Ok(Vec::new());
    }
    let directives = durable_directives(prompt);
    if directives.is_empty() {
        return Ok(Vec::new());
    }

    let path = ensure_file(workspace)?;
    let mut body = std::fs::read_to_string(&path)?;
    let existing = body.to_ascii_lowercase();
    let mut additions: Vec<String> = Vec::new();
    for directive in directives {
        let normalized = directive.to_ascii_lowercase();
        if !existing.contains(&normalized)
            && !additions
                .iter()
                .any(|added| added.eq_ignore_ascii_case(&directive))
        {
            additions.push(directive);
        }
    }
    if additions.is_empty() {
        return Ok(Vec::new());
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    if !body.contains("## Conversation-derived instructions") {
        body.push_str("\n## Conversation-derived instructions\n");
    }
    for directive in &additions {
        body.push_str("\n- ");
        body.push_str(directive);
    }
    body.push('\n');
    if body.len() > MAX_BYTES {
        return Err(ArgoError::Invalid(format!(
            "{} reached its {} KiB safety limit; edit it before adding more instructions",
            path.display(),
            MAX_BYTES / 1024
        )));
    }
    let temporary = path.with_extension("md.tmp");
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, &path)?;
    Ok(additions)
}

fn durable_directives(prompt: &str) -> Vec<String> {
    const SIGNALS: &[&str] = &[
        "always ",
        "never ",
        "please always ",
        "please never ",
        "from now on",
        "for this project",
        "in this project",
        "project instruction",
        "instruction:",
        "instructions:",
        "remember to ",
        "should always ",
        "must always ",
        "default to ",
        "i prefer ",
        "we prefer ",
    ];
    prompt
        .lines()
        .filter_map(|line| {
            let line = line
                .trim()
                .trim_start_matches(|ch: char| ch == '-' || ch == '*' || ch.is_ascii_digit())
                .trim_start_matches(['.', ')', ' '])
                .trim();
            if line.len() < 4 || line.len() > 1_000 || line.starts_with('/') {
                return None;
            }
            let lower = line.to_ascii_lowercase();
            SIGNALS
                .iter()
                .any(|signal| lower.starts_with(signal) || lower.contains(signal))
                .then(|| line.to_string())
        })
        .collect()
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

    #[test]
    fn argo_instructions_are_disabled_by_default_even_when_the_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            ARGO_INSTRUCTIONS_FILE,
            "Always use the project formatter.",
        );
        assert!(!is_enabled(dir.path()));
        assert!(discover(dir.path()).expect("discover").is_empty());
    }

    #[test]
    fn enabling_creates_and_gates_the_project_file_without_deleting_on_disable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = set_enabled(dir.path(), true).expect("enable");
        assert!(path.is_file());
        assert!(is_enabled(dir.path()));
        // The initial comments and heading do not consume prompt context.
        assert!(discover(dir.path()).expect("discover").is_empty());

        std::fs::write(&path, "# Rules\n\nAlways use pnpm.\n").expect("edit");
        let found = discover(dir.path()).expect("discover");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, ARGO_INSTRUCTIONS_FILE);

        set_enabled(dir.path(), false).expect("disable");
        assert!(!is_enabled(dir.path()));
        assert!(
            path.is_file(),
            "disable must preserve user-authored content"
        );
        assert!(discover(dir.path()).expect("discover").is_empty());
    }

    #[test]
    fn enabling_preserves_a_custom_argo_gitignore_and_ignores_only_the_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let argo_dir = dir.path().join(argo_core::ARGO_WORKSPACE_DIR);
        std::fs::create_dir_all(&argo_dir).expect("argo dir");
        let ignore = argo_dir.join(".gitignore");
        std::fs::write(&ignore, "skills-staged/\n").expect("custom ignore");

        set_enabled(dir.path(), true).expect("enable");
        let body = std::fs::read_to_string(ignore).expect("read");
        assert!(body.contains("skills-staged/"));
        assert!(body.lines().any(|line| line == ENABLED_MARKER));
    }

    #[test]
    fn enabled_projects_capture_only_explicitly_durable_directives_and_deduplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        set_enabled(dir.path(), true).expect("enable");
        let captured = capture_user_directives(
            dir.path(),
            "Fix the failing test.\nFrom now on, always run cargo fmt before committing.",
        )
        .expect("capture");
        assert_eq!(
            captured,
            vec!["From now on, always run cargo fmt before committing."]
        );
        assert!(capture_user_directives(
            dir.path(),
            "From now on, always run cargo fmt before committing."
        )
        .expect("deduplicate")
        .is_empty());
        let body = std::fs::read_to_string(instructions_path(dir.path())).expect("read");
        assert_eq!(body.matches("always run cargo fmt").count(), 1);
        assert!(!body.contains("Fix the failing test"));
    }

    #[test]
    fn disabled_projects_never_capture_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let captured = capture_user_directives(dir.path(), "Always use tabs.").expect("capture");
        assert!(captured.is_empty());
        assert!(!instructions_path(dir.path()).exists());
    }
}
