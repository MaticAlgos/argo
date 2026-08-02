//! Skill staging.
//!
//! Selected skills are copied into Argo's user-level cache before a run, and the
//! agent is pointed at the copy. This is deliberately a real copy rather than a
//! symlink: symlink semantics differ across filesystems, and an agent that edits a
//! linked file would mutate the user's source skill. Keeping the cache outside the
//! workspace also avoids adding runtime-only `.argo` directories to every project.

use argo_core::error::{ArgoError, Result};
use argo_core::sha256_hex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::skills::Skill;

const LEGACY_IGNORE: &str = "# Created by Argo. Staged run inputs, not source.\n*\n";

/// A staged copy of one skill.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedSkill {
    /// Skill name.
    pub name: String,
    /// Absolute path of the staged copy.
    pub path: PathBuf,
}

impl StagedSkill {
    /// Absolute instructions file exposed to the selected CLI.
    pub fn instructions_path(&self) -> PathBuf {
        self.path.join("SKILL.md")
    }
}

/// Copies `skills` into Argo's user-level `cache_root`.
///
/// The destination name includes a hash of the source path so two skills sharing a
/// name from different roots cannot collide in the staging tree.
pub fn stage(cache_root: &Path, skills: &[Skill]) -> Result<Vec<StagedSkill>> {
    if skills.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(cache_root)?;

    let mut staged = Vec::new();
    for skill in skills {
        let hash = sha256_hex(&skill.dir.to_string_lossy());
        let dir_name = format!("{}-{}", skill.name, &hash[..8]);
        let destination = cache_root.join(&dir_name);

        // Compare the complete tree, not only SKILL.md: references, scripts, and
        // assets are part of a skill too. This also restores a cached copy if an
        // agent changed it during an earlier run.
        if needs_refresh(&skill.dir, &destination)? {
            if destination.exists() {
                std::fs::remove_dir_all(&destination)?;
            }
            copy_tree(&skill.dir, &destination)?;
        }

        staged.push(StagedSkill {
            name: skill.name.clone(),
            path: destination,
        });
    }
    Ok(staged)
}

/// Removes the project-local cache written by Argo v0.1.3 and earlier.
///
/// User-authored `.argo/skills` and custom `.argo` files are never removed. The
/// directory itself is deleted only when the legacy cache and Argo's exact
/// generated ignore file were its final contents.
pub fn cleanup_legacy_workspace_cache(workspace: &Path) -> Result<()> {
    let argo_dir = workspace.join(argo_core::ARGO_WORKSPACE_DIR);
    let legacy_cache = argo_dir.join("skills-staged");
    if legacy_cache.is_dir() {
        std::fs::remove_dir_all(&legacy_cache)?;
    }

    let ignore = argo_dir.join(".gitignore");
    let only_generated_ignore = std::fs::read_to_string(&ignore)
        .ok()
        .is_some_and(|body| body == LEGACY_IGNORE)
        && std::fs::read_dir(&argo_dir)
            .ok()
            .is_some_and(|mut entries| {
                entries.all(|entry| entry.is_ok_and(|entry| entry.path() == ignore))
            });
    if only_generated_ignore {
        std::fs::remove_file(&ignore)?;
        std::fs::remove_dir(&argo_dir)?;
    }
    Ok(())
}

/// True when any file, directory, script, reference, or asset differs.
fn needs_refresh(source: &Path, destination: &Path) -> Result<bool> {
    if !destination.is_dir() {
        return Ok(true);
    }
    Ok(tree_fingerprint(source)? != tree_fingerprint(destination)?)
}

/// Content fingerprint for a complete skill tree, independent of traversal order.
fn tree_fingerprint(root: &Path) -> Result<String> {
    let mut entries = walkdir::WalkDir::new(root)
        .max_depth(16)
        .follow_links(true)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ArgoError::Io(format!("walk skill tree: {error}")))?;
    entries.retain(|entry| entry.path() != root);
    entries.sort_by(|left, right| {
        left.path()
            .strip_prefix(root)
            .unwrap_or(left.path())
            .cmp(right.path().strip_prefix(root).unwrap_or(right.path()))
    });

    let mut hasher = Sha256::new();
    for entry in entries {
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| ArgoError::Io(format!("relativize skill path: {error}")))?;
        let relative = relative.to_string_lossy();
        hash_field(&mut hasher, relative.as_bytes());
        if entry.file_type().is_dir() {
            hash_field(&mut hasher, b"directory");
        } else {
            hash_field(&mut hasher, b"file");
            let body = std::fs::read(entry.path()).map_err(|error| {
                ArgoError::Io(format!(
                    "read skill file {}: {error}",
                    entry.path().display()
                ))
            })?;
            hash_field(&mut hasher, &body);
        }
    }

    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Length-prefixing makes path/content boundaries unambiguous in the digest.
fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
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
            "- {} — {}\n  instructions: {}",
            entry.name,
            description,
            entry.instructions_path().display()
        ));
    }
    lines.push(
        "Read a skill's SKILL.md before following it. Instruction paths are absolute Argo cache paths."
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
        let cache = dir.path().join("argo-data/staging/skills");

        let staged = stage(&cache, &[skill(source, "pr-review")]).expect("stage");
        assert_eq!(staged.len(), 1);
        assert!(staged[0].path.join("SKILL.md").exists());
        assert!(
            staged[0].path.join("references/extra.md").exists(),
            "reference files must travel with the skill"
        );
        assert!(staged[0].path.starts_with(cache));
    }

    #[test]
    fn staging_is_a_copy_so_edits_cannot_reach_the_source() {
        // An agent that rewrites a staged file must not corrupt the user's skill.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "deploy");
        let cache = dir.path().join("cache");

        let staged = stage(&cache, &[skill(source.clone(), "deploy")]).expect("stage");
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
        let cache = dir.path().join("cache");

        let staged = stage(&cache, &[skill(a, "shared"), skill(b, "shared")]).expect("stage");
        assert_eq!(staged.len(), 2);
        assert_ne!(staged[0].path, staged[1].path);
    }

    #[test]
    fn staging_never_writes_inside_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "ignored");
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        let cache = dir.path().join("argo-data/staging/skills");

        stage(&cache, &[skill(source, "ignored")]).expect("stage");

        assert!(!workspace.join(".argo").exists());
        assert!(cache.is_dir());
    }

    #[test]
    fn cleanup_removes_only_the_legacy_generated_project_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let argo_dir = dir.path().join(".argo");
        std::fs::create_dir_all(argo_dir.join("skills-staged/old")).expect("legacy cache");
        std::fs::write(argo_dir.join("skills-staged/old/SKILL.md"), "old").expect("old skill");
        std::fs::write(argo_dir.join(".gitignore"), LEGACY_IGNORE).expect("ignore");

        cleanup_legacy_workspace_cache(dir.path()).expect("cleanup");

        assert!(!argo_dir.exists());
    }

    #[test]
    fn cleanup_preserves_user_authored_project_skills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let argo_dir = dir.path().join(".argo");
        std::fs::create_dir_all(argo_dir.join("skills-staged/old")).expect("legacy cache");
        std::fs::create_dir_all(argo_dir.join("skills/custom")).expect("project skill");
        std::fs::write(argo_dir.join("skills/custom/SKILL.md"), "custom").expect("skill");
        std::fs::write(argo_dir.join(".gitignore"), LEGACY_IGNORE).expect("ignore");

        cleanup_legacy_workspace_cache(dir.path()).expect("cleanup");

        assert!(!argo_dir.join("skills-staged").exists());
        assert!(argo_dir.join("skills/custom/SKILL.md").is_file());
        assert!(argo_dir.join(".gitignore").is_file());
    }

    #[test]
    fn unchanged_skills_are_not_recopied() {
        // With dozens of installed skills, re-copying every turn is the difference
        // between a responsive turn and a sluggish one.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "stable");
        let cache = dir.path().join("cache");

        let first = stage(&cache, &[skill(source.clone(), "stable")]).expect("first");
        let manifest = first[0].instructions_path();
        let modified = std::fs::metadata(&manifest)
            .and_then(|metadata| metadata.modified())
            .expect("modified");
        std::thread::sleep(std::time::Duration::from_millis(20));

        stage(&cache, &[skill(source, "stable")]).expect("second");
        assert_eq!(
            std::fs::metadata(manifest)
                .and_then(|metadata| metadata.modified())
                .expect("modified"),
            modified,
            "unchanged skill must not be recopied"
        );
    }

    #[test]
    fn restaging_refreshes_an_edited_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "iterate");
        let cache = dir.path().join("cache");

        stage(&cache, &[skill(source.clone(), "iterate")]).expect("first");
        std::fs::write(source.join("SKILL.md"), "---\nname: iterate\n---\nUPDATED").expect("write");
        let staged = stage(&cache, &[skill(source, "iterate")]).expect("second");

        let content = std::fs::read_to_string(staged[0].path.join("SKILL.md")).expect("read");
        assert!(content.contains("UPDATED"));
    }

    #[test]
    fn restaging_refreshes_changed_side_files_and_removes_deleted_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "complete-tree");
        let cache = dir.path().join("cache");
        let side_file = source.join("references/extra.md");

        stage(&cache, &[skill(source.clone(), "complete-tree")]).expect("first");
        std::fs::write(&side_file, "updated reference").expect("edit reference");
        let staged = stage(&cache, &[skill(source.clone(), "complete-tree")]).expect("updated");
        assert_eq!(
            std::fs::read_to_string(staged[0].path.join("references/extra.md"))
                .expect("staged reference"),
            "updated reference"
        );

        std::fs::remove_file(side_file).expect("remove reference");
        let staged = stage(&cache, &[skill(source, "complete-tree")]).expect("removed");
        assert!(!staged[0].path.join("references/extra.md").exists());
    }

    #[test]
    fn restaging_repairs_a_cache_modified_by_an_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "repair");
        let cache = dir.path().join("cache");
        let first = stage(&cache, &[skill(source.clone(), "repair")]).expect("first");
        std::fs::write(first[0].instructions_path(), "MUTATED").expect("mutate cache");

        let staged = stage(&cache, &[skill(source, "repair")]).expect("repair");
        let body = std::fs::read_to_string(staged[0].instructions_path()).expect("read");
        assert!(body.contains("name: repair"));
        assert!(!body.contains("MUTATED"));
    }

    #[test]
    fn staging_nothing_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("cache");
        assert!(stage(&cache, &[]).expect("stage").is_empty());
        assert!(!cache.exists());
    }

    #[test]
    fn prompt_section_lists_names_paths_and_descriptions_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = make_source(&dir.path().join("src"), "pr-review");
        let cache = dir.path().join("cache");
        let skills = vec![skill(source, "pr-review")];
        let staged = stage(&cache, &skills).expect("stage");

        let section = render_prompt_section(&staged, &skills);
        assert!(section.contains("## Available skills"));
        assert!(section.contains("pr-review — does pr-review"));
        assert!(section.contains("/SKILL.md"));
        assert!(section.contains(&cache.display().to_string()));
        // The body is not inlined; the agent reads it on demand.
        assert!(!section.contains("body"));
    }

    #[test]
    fn empty_prompt_section_when_nothing_is_staged() {
        assert_eq!(render_prompt_section(&[], &[]), "");
    }
}
