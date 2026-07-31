//! Skill discovery.
//!
//! A skill is a directory containing `SKILL.md` with YAML frontmatter — the
//! portable Agent Skills format that Claude, Codex, Kiro, and OpenCode all read.
//! Argo discovers skills the user already has, from every root those CLIs use, so
//! adopting Argo does not mean re-installing anything.
//!
//! Precedence runs most-specific first: a workspace skill shadows a global one of
//! the same name, and Argo's own roots outrank a vendor's. Shadowing is reported
//! rather than hidden, because two skills with one name is usually a mistake the
//! user wants to know about.

use argo_core::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where a discovered skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    /// `.argo/skills` in the workspace.
    ArgoWorkspace,
    /// Argo's managed user skills.
    ArgoUser,
    /// A vendor's workspace-local skills directory.
    VendorWorkspace,
    /// A vendor's global skills directory.
    VendorGlobal,
}

impl SkillOrigin {
    /// Human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ArgoWorkspace => "workspace (argo)",
            Self::ArgoUser => "user (argo)",
            Self::VendorWorkspace => "workspace",
            Self::VendorGlobal => "global",
        }
    }
}

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// Canonical name, from frontmatter.
    pub name: String,
    /// When to use it, from frontmatter.
    pub description: String,
    /// Directory containing `SKILL.md`.
    pub dir: PathBuf,
    /// Where it was found.
    pub origin: SkillOrigin,
    /// Vendor directory it came from, for display.
    pub source: String,
    /// Names of skills this one shadowed.
    pub shadows: Vec<String>,
}

/// A parsed `SKILL.md` frontmatter.
#[derive(Debug, Clone, PartialEq)]
struct Frontmatter {
    name: String,
    description: String,
}

/// Validates a skill name against the Agent Skills rules.
///
/// Lowercase alphanumeric with single hyphens, 1-64 characters. Enforced because
/// the name doubles as a slash-command and a staging directory component.
pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Extracts `name` and `description` from `SKILL.md` content.
///
/// Only these two fields are required by the format; unknown keys are ignored so
/// a vendor's extra metadata never makes a skill undiscoverable.
fn parse_frontmatter(content: &str) -> Option<Frontmatter> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let block = &rest[..end];

    let mut name = None;
    let mut description = None;
    let mut current: Option<&str> = None;
    let mut buffer = String::new();

    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // A new top-level key ends any multi-line value being accumulated.
        if let Some((key, value)) = split_key(trimmed) {
            flush(current, &mut buffer, &mut name, &mut description);
            current = None;
            let value = value.trim();
            match key {
                "name" => name = Some(unquote(value).to_string()),
                "description" => {
                    if value == "|" || value == ">" || value.is_empty() {
                        // Block scalar: the value continues on following lines.
                        current = Some("description");
                        buffer.clear();
                    } else {
                        description = Some(unquote(value).to_string());
                    }
                }
                _ => {}
            }
            continue;
        }

        if current.is_some() {
            if !buffer.is_empty() {
                buffer.push(' ');
            }
            buffer.push_str(trimmed);
        }
    }
    flush(current, &mut buffer, &mut name, &mut description);

    Some(Frontmatter {
        name: name?,
        description: description.unwrap_or_default(),
    })
}

fn flush(
    current: Option<&str>,
    buffer: &mut String,
    _name: &mut Option<String>,
    description: &mut Option<String>,
) {
    if current == Some("description") && !buffer.is_empty() {
        *description = Some(buffer.trim().to_string());
        buffer.clear();
    }
}

/// Splits `key: value` at the top level of a frontmatter block.
fn split_key(line: &str) -> Option<(&str, &str)> {
    // Indented lines are continuations or nested keys, not new top-level keys.
    if line.starts_with(' ') || line.starts_with('-') {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    if key.is_empty() || key.contains(' ') {
        return None;
    }
    Some((key, value))
}

/// Strips matching surrounding quotes.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return &value[1..value.len() - 1];
    }
    value
}

/// Reads one skill directory.
fn load_skill(dir: &Path, origin: SkillOrigin, source: &str) -> Option<Skill> {
    let manifest = dir.join("SKILL.md");
    let content = std::fs::read_to_string(&manifest).ok()?;
    let frontmatter = parse_frontmatter(&content)?;

    // The directory name is authoritative when frontmatter disagrees, because the
    // path is what the agent is told to read.
    let dir_name = dir.file_name()?.to_string_lossy().to_string();
    let name = if frontmatter.name == dir_name {
        frontmatter.name
    } else if is_valid_name(&dir_name) {
        dir_name
    } else {
        frontmatter.name
    };

    if !is_valid_name(&name) {
        return None;
    }

    Some(Skill {
        name,
        description: frontmatter.description,
        dir: dir.to_path_buf(),
        origin,
        source: source.to_string(),
        shadows: Vec::new(),
    })
}

/// Vendor-relative skill directories, in precedence order.
const WORKSPACE_ROOTS: &[(&str, &str)] = &[
    (".argo/skills", "argo"),
    (".claude/skills", "claude"),
    (".agents/skills", "agents"),
    (".opencode/skills", "opencode"),
    (".kiro/skills", "kiro"),
    (".codex/skills", "codex"),
];

/// Home-relative skill directories, in precedence order.
const GLOBAL_ROOTS: &[(&str, &str)] = &[
    (".claude/skills", "claude"),
    (".agents/skills", "agents"),
    (".config/opencode/skills", "opencode"),
    (".kiro/skills", "kiro"),
    (".codex/skills", "codex"),
];

/// Discovers every skill visible to `workspace`, applying precedence.
///
/// `argo_user_root` is Argo's own managed skills directory, which outranks vendor
/// globals but not workspace skills.
pub fn discover(
    workspace: &Path,
    argo_user_root: &Path,
    home: Option<&Path>,
) -> Result<Vec<Skill>> {
    let mut ordered: Vec<Skill> = Vec::new();

    // Workspace first: most specific wins.
    for (relative, source) in WORKSPACE_ROOTS {
        let origin = if *source == "argo" {
            SkillOrigin::ArgoWorkspace
        } else {
            SkillOrigin::VendorWorkspace
        };
        collect_into(&mut ordered, &workspace.join(relative), origin, source);
    }

    // Argo's managed user skills.
    collect_into(&mut ordered, argo_user_root, SkillOrigin::ArgoUser, "argo");

    // Vendor globals last.
    if let Some(home) = home {
        for (relative, source) in GLOBAL_ROOTS {
            collect_into(
                &mut ordered,
                &home.join(relative),
                SkillOrigin::VendorGlobal,
                source,
            );
        }
    }

    Ok(resolve_precedence(ordered))
}

/// Reads every skill directory directly under `root`.
fn collect_into(out: &mut Vec<Skill>, root: &Path, origin: SkillOrigin, source: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut found: Vec<Skill> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| load_skill(&entry.path(), origin, source))
        .collect();
    // Stable order so listings do not shuffle between runs.
    found.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(found);
}

/// Keeps the first occurrence of each name and records what it shadowed.
fn resolve_precedence(ordered: Vec<Skill>) -> Vec<Skill> {
    let mut winners: Vec<Skill> = Vec::new();
    for candidate in ordered {
        match winners.iter_mut().find(|s| s.name == candidate.name) {
            Some(winner) => {
                // Surfaced rather than dropped silently: a duplicate name usually
                // means the user has two copies and expects one to apply.
                winner.shadows.push(format!(
                    "{} ({})",
                    candidate.dir.display(),
                    candidate.origin.label()
                ));
            }
            None => winners.push(candidate),
        }
    }
    winners.sort_by(|a, b| a.name.cmp(&b.name));
    winners
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n"),
        )
        .expect("write");
    }

    #[test]
    fn validates_names_per_the_agent_skills_rules() {
        assert!(is_valid_name("pr-review"));
        assert!(is_valid_name("a"));
        assert!(is_valid_name("git-release-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name("double--hyphen"));
        assert!(!is_valid_name("UpperCase"));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("under_score"));
        assert!(!is_valid_name(&"x".repeat(65)));
    }

    #[test]
    fn parses_simple_frontmatter() {
        let parsed = parse_frontmatter("---\nname: pr-review\ndescription: Review PRs\n---\nbody")
            .expect("parse");
        assert_eq!(parsed.name, "pr-review");
        assert_eq!(parsed.description, "Review PRs");
    }

    #[test]
    fn parses_block_scalar_descriptions() {
        // The format allows multi-line descriptions; losing them would degrade the
        // agent's ability to pick the right skill.
        let content = "---\nname: deploy\ndescription: |\n  Deploy the service.\n  Use when shipping.\n---\nbody";
        let parsed = parse_frontmatter(content).expect("parse");
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.description, "Deploy the service. Use when shipping.");
    }

    #[test]
    fn ignores_unknown_frontmatter_keys() {
        let content =
            "---\nname: x\nlicense: MIT\ncompatibility: opencode\nmetadata:\n  team: core\ndescription: D\n---\n";
        let parsed = parse_frontmatter(content).expect("parse");
        assert_eq!(parsed.name, "x");
        assert_eq!(parsed.description, "D");
    }

    #[test]
    fn strips_quotes_from_values() {
        let parsed = parse_frontmatter("---\nname: \"quoted\"\ndescription: 'single'\n---\n")
            .expect("parse");
        assert_eq!(parsed.name, "quoted");
        assert_eq!(parsed.description, "single");
    }

    #[test]
    fn rejects_content_without_frontmatter() {
        assert!(parse_frontmatter("# Just a heading\n").is_none());
        assert!(parse_frontmatter("---\nno terminator\n").is_none());
    }

    #[test]
    fn discovers_skills_from_every_supported_vendor_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        let home = dir.path().join("home");
        let argo_user = dir.path().join("argo-user");

        write_skill(&workspace.join(".claude/skills"), "claude-local", "c");
        write_skill(&workspace.join(".agents/skills"), "agents-local", "a");
        write_skill(&workspace.join(".opencode/skills"), "opencode-local", "o");
        write_skill(&workspace.join(".kiro/skills"), "kiro-local", "k");
        write_skill(&home.join(".claude/skills"), "claude-global", "cg");
        write_skill(&home.join(".config/opencode/skills"), "oc-global", "og");
        write_skill(&argo_user, "argo-managed", "am");

        let found = discover(&workspace, &argo_user, Some(&home)).expect("discover");
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();

        for expected in [
            "agents-local",
            "argo-managed",
            "claude-global",
            "claude-local",
            "kiro-local",
            "oc-global",
            "opencode-local",
        ] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }
    }

    #[test]
    fn workspace_skills_shadow_global_ones_and_the_conflict_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        let home = dir.path().join("home");
        let argo_user = dir.path().join("argo-user");

        write_skill(
            &workspace.join(".claude/skills"),
            "shared",
            "workspace copy",
        );
        write_skill(&home.join(".claude/skills"), "shared", "global copy");

        let found = discover(&workspace, &argo_user, Some(&home)).expect("discover");
        let shared: Vec<&Skill> = found.iter().filter(|s| s.name == "shared").collect();
        assert_eq!(shared.len(), 1, "only one skill may win a name");
        assert_eq!(shared[0].description, "workspace copy");
        assert_eq!(shared[0].origin, SkillOrigin::VendorWorkspace);
        // The loser is reported so the user can resolve the duplicate.
        assert_eq!(shared[0].shadows.len(), 1);
        assert!(shared[0].shadows[0].contains("global"));
    }

    #[test]
    fn argo_workspace_skills_outrank_vendor_workspace_skills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        write_skill(&workspace.join(".argo/skills"), "dup", "argo copy");
        write_skill(&workspace.join(".claude/skills"), "dup", "claude copy");

        let found = discover(&workspace, &dir.path().join("u"), None).expect("discover");
        let winner = found.iter().find(|s| s.name == "dup").expect("dup");
        assert_eq!(winner.origin, SkillOrigin::ArgoWorkspace);
        assert_eq!(winner.description, "argo copy");
    }

    #[test]
    fn argo_user_skills_outrank_vendor_globals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        let home = dir.path().join("home");
        let argo_user = dir.path().join("argo-user");
        write_skill(&argo_user, "dup", "argo user copy");
        write_skill(&home.join(".claude/skills"), "dup", "vendor global copy");

        let found = discover(&workspace, &argo_user, Some(&home)).expect("discover");
        let winner = found.iter().find(|s| s.name == "dup").expect("dup");
        assert_eq!(winner.origin, SkillOrigin::ArgoUser);
    }

    #[test]
    fn missing_roots_are_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = discover(
            &dir.path().join("nonexistent"),
            &dir.path().join("also-missing"),
            Some(&dir.path().join("no-home")),
        )
        .expect("discover");
        assert!(found.is_empty());
    }

    #[test]
    fn directories_without_a_manifest_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo/.claude/skills/not-a-skill");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("README.md"), "nope").expect("write");
        let found =
            discover(&dir.path().join("repo"), &dir.path().join("u"), None).expect("discover");
        assert!(found.is_empty());
    }

    #[test]
    fn invalid_names_are_rejected_rather_than_staged() {
        // A name that is not a safe path component must never reach staging.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo/.claude/skills");
        let bad = root.join("Bad_Name");
        std::fs::create_dir_all(&bad).expect("mkdir");
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Bad_Name\ndescription: d\n---\n",
        )
        .expect("write");
        let found =
            discover(&dir.path().join("repo"), &dir.path().join("u"), None).expect("discover");
        assert!(found.is_empty());
    }

    #[test]
    fn discovery_order_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        for name in ["zeta", "alpha", "mid"] {
            write_skill(&workspace.join(".claude/skills"), name, "d");
        }
        let first = discover(&workspace, &dir.path().join("u"), None).expect("discover");
        let second = discover(&workspace, &dir.path().join("u"), None).expect("discover");
        let names: Vec<&str> = first.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
        assert_eq!(first, second);
    }
}
