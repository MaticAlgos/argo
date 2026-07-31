//! Skill staging.
//!
//! Selected skills are copied into a project-private directory before a run, and
//! the agent is pointed at the copy. This is deliberately a real copy rather than
//! a symlink, for two reasons OpenDesign learned the hard way: symlink semantics
//! differ across filesystems, and an agent that edits a linked file would mutate
//! the user's source skill.

use argo_core::error::{ArgoError, Result};
use argo_core::sha256_hex;
use std::path::{Path, PathBuf};

use crate::skills::Skill;

/// A staged copy of one skill.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedSkill {
    /// Skill name.
    pub name: String,
    /// Absolute path of the staged copy.
    pub path: PathBuf,
    /// Path relative to the workspace, for use in prompts.
    pub relative: String,
}

/// Copies `skills` into `<workspace>/.argo/skills-staged/`.
///
/// The destination name includes a hash of the source path so two skills sharing a
/// name from different roots cannot collide in the staging tree.
pub fn stage(workspace: &Path, skills: &[Skill]) -> Result<Vec<StagedSkill>> {
    if skills.is_empty() {
        return Ok(Vec::new());
    }
    let argo_dir = workspace.join(argo_core::ARGO_WORKSPACE_DIR);
    let root = argo_dir.join("skills-staged");
    std::fs::create_dir_all(&root)?;

    // Argo's working directory is build output, not source. Writing the ignore
    // rule here keeps staged copies out of the user's commits and diffs.
    let ignore = argo_dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(
            &ignore,
            "# Created by Argo. Staged run inputs, not source.\n*\n",
        )?;
    }

    let mut staged = Vec::new();
    for skill in skills {
        let hash = sha256_hex(&skill.dir.to_string_lossy());
        let dir_name = format!("{}-{}", skill.name, &hash[..8]);
        let destination = root.join(&dir_name);

        // Copying every discovered skill on every turn is the difference between
        // a fast turn and a slow one once a user has dozens installed, so an
        // unchanged skill is left alone.
        if needs_refresh(&skill.dir, &destination) {
            if destination.exists() {
                std::fs::remove_dir_all(&destination)?;
            }
            copy_tree(&skill.dir, &destination)?;
        }

        staged.push(StagedSkill {
            name: skill.name.clone(),
            path: destination,
            relative: format!("{}/skills-staged/{dir_name}", argo_core::ARGO_WORKSPACE_DIR),
        });
    }
    Ok(staged)
}

/// True when the staged copy is missing or older than its source manifest.
///
/// Compares the manifest rather than walking the tree: it is the file that changes
/// when a skill is edited, and stat-ing one path per skill keeps this cheap.
fn needs_refresh(source: &Path, destination: &Path) -> bool {
    let staged_manifest = destination.join("SKILL.md");
    if !staged_manifest.exists() {
        return true;
    }
    let source_time = std::fs::metadata(source.join("SKILL.md"))
        .and_then(|m| m.modified())
        .ok();
    let staged_time = std::fs::metadata(&staged_manifest)
        .and_then(|m| m.modified())
        .ok();
    match (source_time, staged_time) {
        (Some(source_time), Some(staged_time)) => source_time > staged_time,
        // If either timestamp is unavailable, re-copy rather than risk serving a
        // stale skill.
        _ => true,
    }
}

/// Recursively copies a directory, dereferencing symlinks.
///
/// Depth is bounded so a cyclic symlink in a user's skill directory cannot spin
/// forever.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    const MAX_DEPTH: usize = 16;
    std::fs::create_dir_all(to)?;

    for entry in walkdir::WalkDir::new(from)
        .max_depth(MAX_DEPTH)
        .follow_links(true)
    {
        let entry = entry.map_err(|e| ArgoError::Io(format!("read skill tree: {e}")))?;
        let relative = entry
            .path()
            .strip_prefix(from)
            .map_err(|e| ArgoError::Io(format!("relativize skill path: {e}")))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)
                .map_err(|e| ArgoError::Io(format!("copy {}: {e}", entry.path().display())))?;
        }
    }
    Ok(())
}

/// Renders the prompt section describing staged skills.
///
/// Only names, descriptions, and paths are inlined. Bodies stay on disk for the
/// agent to read on demand, which keeps a large skill from consuming the context
/// window of a turn that never uses it.
pub fn render_prompt_section(staged: &[StagedSkill], skills: &[Skill]) -> String {
    if staged.is_empty() {
        return String::new();
    }
    let mut lines = vec!["## Available skills".to_string()];
    for entry in staged {
        let description = skills
            .iter()
            .find(|s| s.name == entry.name)
            .map(|s| s.description.as_str())
            .unwrap_or_default();
        lines.push(format!(
            "- {} — {}\n  instructions: {}/SKILL.md",
            entry.name, description, entry.relative
        ));
    }
    lines.push(
        "Read a skill's SKILL.md before following it. Paths are relative to the workspace root."
            .to_string(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillOrigin;

    fn skill(dir: PathBuf, name: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("does {name}"),
            dir,
            origin: SkillOrigin::VendorGlobal,
            source: "claude".into(),
            shadows: vec![],
        }
    }

    fn make_source(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("references")).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\nbody"),
        )
        .expect("write");
        std::fs::write(dir.join("references").join("extra.md"), "detail").expect("write");
        dir
    }

    #[test]
    fn stages_a_skill_with_its_side_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "pr-review");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");

        let staged = stage(&workspace, &[skill(source, "pr-review")]).expect("stage");
        assert_eq!(staged.len(), 1);
        assert!(staged[0].path.join("SKILL.md").exists());
        assert!(
            staged[0].path.join("references/extra.md").exists(),
            "reference files must travel with the skill"
        );
        assert!(staged[0].relative.starts_with(".argo/skills-staged/"));
    }

    #[test]
    fn staging_is_a_copy_so_edits_cannot_reach_the_source() {
        // An agent that rewrites a staged file must not corrupt the user's skill.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "deploy");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");

        let staged = stage(&workspace, &[skill(source.clone(), "deploy")]).expect("stage");
        std::fs::write(staged[0].path.join("SKILL.md"), "OVERWRITTEN").expect("write");

        let original = std::fs::read_to_string(source.join("SKILL.md")).expect("read");
        assert!(original.contains("name: deploy"));
        assert!(!original.contains("OVERWRITTEN"));
    }

    #[test]
    fn same_named_skills_from_different_roots_do_not_collide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = make_source(&dir.path().join("root-a"), "shared");
        let b = make_source(&dir.path().join("root-b"), "shared");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");

        let staged = stage(&workspace, &[skill(a, "shared"), skill(b, "shared")]).expect("stage");
        assert_eq!(staged.len(), 2);
        assert_ne!(staged[0].path, staged[1].path);
    }

    #[test]
    fn staging_writes_a_gitignore_so_copies_never_reach_a_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "ignored");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        stage(&workspace, &[skill(source, "ignored")]).expect("stage");
        let ignore = workspace.join(".argo/.gitignore");
        assert!(ignore.exists());
        assert!(std::fs::read_to_string(ignore).expect("read").contains('*'));
    }

    #[test]
    fn unchanged_skills_are_not_recopied() {
        // With dozens of installed skills, re-copying every turn is the difference
        // between a responsive turn and a sluggish one.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "stable");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");

        let first = stage(&workspace, &[skill(source.clone(), "stable")]).expect("first");
        let marker = first[0].path.join("marker.txt");
        std::fs::write(&marker, "untouched").expect("write");

        stage(&workspace, &[skill(source, "stable")]).expect("second");
        // A wholesale re-copy would have deleted the marker.
        assert!(marker.exists(), "unchanged skill must not be recopied");
    }

    #[test]
    fn restaging_refreshes_an_edited_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "iterate");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");

        stage(&workspace, &[skill(source.clone(), "iterate")]).expect("first");
        // Filesystem timestamps have coarse resolution; wait so the edit is
        // unambiguously newer than the staged copy.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(source.join("SKILL.md"), "---\nname: iterate\n---\nUPDATED").expect("write");
        let staged = stage(&workspace, &[skill(source, "iterate")]).expect("second");

        let content = std::fs::read_to_string(staged[0].path.join("SKILL.md")).expect("read");
        assert!(content.contains("UPDATED"));
    }

    #[test]
    fn staging_nothing_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        assert!(stage(&workspace, &[]).expect("stage").is_empty());
        assert!(!workspace.join(".argo/skills-staged").exists());
    }

    #[test]
    fn prompt_section_lists_names_paths_and_descriptions_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "pr-review");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        let skills = vec![skill(source, "pr-review")];
        let staged = stage(&workspace, &skills).expect("stage");

        let section = render_prompt_section(&staged, &skills);
        assert!(section.contains("## Available skills"));
        assert!(section.contains("pr-review — does pr-review"));
        assert!(section.contains("/SKILL.md"));
        // The body is not inlined; the agent reads it on demand.
        assert!(!section.contains("body"));
    }

    #[test]
    fn empty_prompt_section_when_nothing_is_staged() {
        assert_eq!(render_prompt_section(&[], &[]), "");
    }
}
