//! Persisted TUI startup preferences.
//!
//! A default is deliberately an explicit CLI/model pair. Persisting only a CLI
//! would silently fall back to whatever model that vendor happens to configure,
//! which makes the target shown by Argo misleading.

use argo_core::error::{ArgoError, Result};
use argo_core::ArgoPaths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Selection applied to new conversations when the TUI opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultSelection {
    /// Runtime adapter id.
    pub agent: String,
    /// Concrete model id chosen for that adapter.
    pub model: String,
    /// Optional model-specific reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl DefaultSelection {
    /// Human-readable exact routing target.
    pub fn label(&self) -> String {
        match &self.effort {
            Some(effort) => format!("{}/{} · {effort}", self.agent, self.model),
            None => format!("{}/{}", self.agent, self.model),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_selection: Option<DefaultSelection>,
}

fn path(paths: &ArgoPaths) -> PathBuf {
    paths.root().join("tui-preferences.json")
}

/// Loads the configured startup selection.
pub fn load(paths: &ArgoPaths) -> Result<Option<DefaultSelection>> {
    let path = path(paths);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let preferences: Preferences = serde_json::from_slice(&bytes).map_err(|error| {
        ArgoError::Invalid(format!(
            "invalid TUI preferences at {}: {error}",
            path.display()
        ))
    })?;
    Ok(preferences.default_selection)
}

/// Saves or clears the startup selection atomically.
pub fn save(paths: &ArgoPaths, selection: Option<DefaultSelection>) -> Result<()> {
    paths.ensure_dirs()?;
    let destination = path(paths);
    let temporary = destination.with_extension("json.tmp");
    let payload = serde_json::to_vec_pretty(&Preferences {
        default_selection: selection,
    })?;
    std::fs::write(&temporary, payload)?;
    std::fs::rename(&temporary, &destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_an_exact_agent_model_pair() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("argo-tui-prefs-{nonce}"));
        let paths = ArgoPaths::with_root(&root);
        assert_eq!(load(&paths).expect("missing preferences"), None);

        let configured = DefaultSelection {
            agent: "codex".into(),
            model: "gpt-5.6-sol".into(),
            effort: Some("high".into()),
        };
        save(&paths, Some(configured.clone())).expect("save default");
        assert_eq!(load(&paths).expect("load default"), Some(configured));

        save(&paths, None).expect("clear default");
        assert_eq!(load(&paths).expect("load cleared"), None);
        std::fs::remove_dir_all(root).ok();
    }
}
