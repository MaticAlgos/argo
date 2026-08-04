//! Persisted bridge configuration.
//!
//! Two files, deliberately separate: `telegram.json` holds ordinary settings and
//! is written often, while the bot token lives alone in a `0600` file that is
//! written once. Anyone holding the token can talk to the bot, so it never goes
//! near a file the rest of the config churns.

use argo_core::error::{ArgoError, Result};
use argo_core::ArgoPaths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Serializes every Telegram settings/token mutation in this process.
///
/// The daemon poller and local IPC requests can update the same settings at the
/// same time. Keeping their read-modify-write sequences under one lock prevents
/// an offset write from erasing a newly allowed workspace (and vice versa).
fn mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Environment override for the bot token, checked before the token file.
pub const TOKEN_ENV: &str = "ARGO_TELEGRAM_TOKEN";

/// Persisted bridge state.
///
/// `Default` is written out by hand rather than derived: a derived one would set
/// the two feature flags to `false`, which is the opposite of the `serde`
/// defaults used when a field is missing, and a config created fresh would
/// silently come up with reactions and live editing disabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot username, for display and deep links.
    #[serde(default)]
    pub bot_username: Option<String>,
    /// Telegram user ids allowed to drive Argo.
    ///
    /// Never empty in a configured bridge: an empty allowlist would expose a
    /// full-access agent to anyone who found the bot, so it is treated as
    /// "not yet linked" and refuses everything.
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    /// Workspace roots the bridge may open, in the order they were allowed.
    #[serde(default)]
    pub workspaces: Vec<String>,
    /// Workspace currently in focus.
    #[serde(default)]
    pub active_workspace: Option<String>,
    /// Conversation currently in focus.
    #[serde(default)]
    pub active_conversation: Option<String>,
    /// Next `getUpdates` offset, so a restart does not replay old messages.
    #[serde(default)]
    pub update_offset: i64,
    /// Whether to react to messages with progress emoji.
    #[serde(default = "default_true")]
    pub reactions: bool,
    /// Whether to live-edit the reply bubble while the turn streams.
    #[serde(default = "default_true")]
    pub stream_edits: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_username: None,
            allowed_user_ids: Vec::new(),
            workspaces: Vec::new(),
            active_workspace: None,
            active_conversation: None,
            update_offset: 0,
            reactions: default_true(),
            stream_edits: default_true(),
        }
    }
}

impl TelegramConfig {
    /// True when the bridge has a bot and at least one authorized human.
    pub fn is_linked(&self) -> bool {
        self.bot_username.is_some() && !self.allowed_user_ids.is_empty()
    }

    /// True when `user_id` may drive Argo.
    pub fn allows(&self, user_id: i64) -> bool {
        // An unlinked bridge authorizes nobody. This is the check that stands
        // between a stranger and a full-access agent, so it fails closed.
        !self.allowed_user_ids.is_empty() && self.allowed_user_ids.contains(&user_id)
    }

    /// Authorizes a user, ignoring duplicates.
    pub fn allow_user(&mut self, user_id: i64) {
        if !self.allowed_user_ids.contains(&user_id) {
            self.allowed_user_ids.push(user_id);
        }
    }

    /// Records the current bot and optionally clears state that cannot cross a
    /// token boundary.
    ///
    /// A replacement bot must be linked to a human and workspace again, starts
    /// with its own update sequence, and must not inherit an active conversation.
    pub fn bind_bot(&mut self, username: String, replacement: bool) {
        self.bot_username = Some(username);
        if replacement {
            self.allowed_user_ids.clear();
            self.workspaces.clear();
            self.active_workspace = None;
            self.active_conversation = None;
            self.update_offset = 0;
        }
    }

    /// True when `root` is an allowlisted workspace.
    ///
    /// Compared against the recorded roots rather than the filesystem: the
    /// bridge must never treat a chat message as a path to resolve.
    pub fn allows_workspace(&self, root: &str) -> bool {
        self.workspaces.iter().any(|allowed| allowed == root)
    }

    /// Allowlists a workspace root, making it the active one if it is the first.
    ///
    /// Returns the outcome so a caller can say what happened. Allowing a second
    /// directory deliberately does **not** redirect the chat: a phone mid-turn
    /// against one workspace must not be silently repointed by something typed
    /// in a terminal somewhere else. The switch is [`Self::active_workspace`],
    /// driven from the chat itself.
    pub fn allow_workspace(&mut self, root: impl Into<String>) -> Allowed {
        let root = root.into();
        let known = self.workspaces.contains(&root);
        if !known {
            self.workspaces.push(root.clone());
        }
        if self.active_workspace.is_none() {
            self.active_workspace = Some(root);
            return Allowed::Activated;
        }
        if known {
            Allowed::AlreadyKnown
        } else {
            Allowed::AddedInactive
        }
    }
}

/// What [`TelegramConfig::allow_workspace`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// The root was added and became the active workspace.
    Activated,
    /// The root was added, but another workspace stays active.
    AddedInactive,
    /// The root was already on the allowlist.
    AlreadyKnown,
}

/// Path of the settings file.
pub fn config_path(paths: &ArgoPaths) -> PathBuf {
    paths.root().join("telegram.json")
}

/// Path of the token file.
pub fn token_path(paths: &ArgoPaths) -> PathBuf {
    paths.root().join("telegram-token")
}

/// Loads the configuration, or `None` when the bridge was never set up.
pub fn load(paths: &ArgoPaths) -> Result<Option<TelegramConfig>> {
    let _guard = mutation_lock()
        .lock()
        .map_err(|_| ArgoError::Io("Telegram config lock was poisoned".into()))?;
    load_unlocked(paths)
}

fn load_unlocked(paths: &ArgoPaths) -> Result<Option<TelegramConfig>> {
    let path = config_path(paths);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        ArgoError::Invalid(format!(
            "invalid Telegram config at {}: {error}",
            path.display()
        ))
    })
}

/// Saves the configuration atomically with owner-only permissions.
pub fn save(paths: &ArgoPaths, config: &TelegramConfig) -> Result<()> {
    let _guard = mutation_lock()
        .lock()
        .map_err(|_| ArgoError::Io("Telegram config lock was poisoned".into()))?;
    save_unlocked(paths, config)
}

/// Applies one serialized read-modify-write operation to the configuration.
pub fn mutate<T>(paths: &ArgoPaths, change: impl FnOnce(&mut TelegramConfig) -> T) -> Result<T> {
    let _guard = mutation_lock()
        .lock()
        .map_err(|_| ArgoError::Io("Telegram config lock was poisoned".into()))?;
    let mut config = load_unlocked(paths)?.unwrap_or_default();
    let output = change(&mut config);
    save_unlocked(paths, &config)?;
    Ok(output)
}

fn save_unlocked(paths: &ArgoPaths, config: &TelegramConfig) -> Result<()> {
    atomic_private_write(
        paths,
        &config_path(paths),
        &serde_json::to_vec_pretty(config)?,
    )
}

/// Reads the bot token from the environment or the token file.
pub fn load_token(paths: &ArgoPaths) -> Result<Option<String>> {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    match std::fs::read_to_string(token_path(paths)) {
        Ok(token) => {
            let token = token.trim().to_string();
            Ok((!token.is_empty()).then_some(token))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Writes the bot token atomically with owner-only permissions.
pub fn save_token(paths: &ArgoPaths, token: &str) -> Result<()> {
    let _guard = mutation_lock()
        .lock()
        .map_err(|_| ArgoError::Io("Telegram config lock was poisoned".into()))?;
    atomic_private_write(
        paths,
        &token_path(paths),
        format!("{}\n", token.trim()).as_bytes(),
    )
}

/// Removes the stored token and every Telegram setting.
pub fn remove(paths: &ArgoPaths) -> Result<()> {
    let _guard = mutation_lock()
        .lock()
        .map_err(|_| ArgoError::Io("Telegram config lock was poisoned".into()))?;
    for path in [config_path(paths), token_path(paths)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Legacy alias for [`remove`].
pub fn reset(paths: &ArgoPaths) -> Result<()> {
    remove(paths)
}

/// Writes through a random `create_new` file and atomically replaces `path`.
///
/// `create_new` prevents following a pre-planted temporary symlink; rename then
/// replaces a destination symlink itself instead of following it to its target.
fn atomic_private_write(paths: &ArgoPaths, path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;

    paths.ensure_dirs()?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("telegram");
    let temporary =
        path.with_file_name(format!(".{name}.argo-{}.tmp", argo_core::RunId::generate()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(body)?;
        file.sync_all()?;
        restrict(&temporary)?;
        std::fs::rename(&temporary, path)?;
        restrict(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Restricts a file to the owner.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// A basic shape check on a pasted bot token.
///
/// Catches the common paste mistakes — a username, a truncated copy, stray
/// quotes — before a network call, so the wizard can say what is wrong.
pub fn looks_like_token(candidate: &str) -> bool {
    let candidate = candidate.trim();
    let Some((id, secret)) = candidate.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit())
        && secret.len() >= 20
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (ArgoPaths, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        (ArgoPaths::with_root(dir.path().join("data")), dir)
    }

    #[test]
    fn a_missing_config_is_not_an_error() {
        let (paths, _dir) = paths();
        assert_eq!(load(&paths).expect("load"), None);
        assert_eq!(load_token(&paths).expect("token"), None);
    }

    #[test]
    fn config_and_token_round_trip_separately() {
        let (paths, _dir) = paths();
        let mut config = TelegramConfig {
            bot_username: Some("argo_test_bot".into()),
            ..Default::default()
        };
        config.allow_user(42);
        config.allow_workspace("/repo");
        save(&paths, &config).expect("save");
        save_token(&paths, " 123:abcdefghijklmnopqrstuvwx \n").expect("save token");

        assert_eq!(load(&paths).expect("load"), Some(config));
        assert_eq!(
            load_token(&paths).expect("token").as_deref(),
            Some("123:abcdefghijklmnopqrstuvwx")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (paths, _dir) = paths();
        save_token(&paths, "123:abcdefghijklmnopqrstuvwx").expect("save token");
        let mode = std::fs::metadata(token_path(&paths))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "token must not be group/world readable"
        );
    }

    #[test]
    fn a_fresh_config_has_reactions_and_live_editing_on() {
        // A derived Default would make these false, quietly shipping a bridge
        // with its two most visible features switched off.
        let fresh = TelegramConfig::default();
        assert!(fresh.reactions, "reactions must default on");
        assert!(fresh.stream_edits, "live editing must default on");

        // And the serde defaults must agree, so an older config file missing the
        // fields behaves the same as a new one.
        let from_minimal: TelegramConfig =
            serde_json::from_str(r#"{"bot_username":"b"}"#).expect("parse");
        assert!(from_minimal.reactions);
        assert!(from_minimal.stream_edits);
    }

    #[test]
    fn an_unlinked_bridge_authorizes_nobody() {
        // The failure that matters: an empty allowlist must never mean "allow
        // everyone", because the agent behind it has full access.
        let config = TelegramConfig {
            bot_username: Some("argo_test_bot".into()),
            ..Default::default()
        };
        assert!(!config.is_linked());
        assert!(!config.allows(0));
        assert!(!config.allows(42));
    }

    #[test]
    fn only_allowlisted_users_are_authorized() {
        let mut config = TelegramConfig::default();
        config.allow_user(42);
        config.allow_user(42);
        assert_eq!(config.allowed_user_ids, vec![42]);
        assert!(config.allows(42));
        assert!(!config.allows(43));
    }

    #[test]
    fn replacement_bot_resets_bot_specific_state() {
        let mut config = TelegramConfig::default();
        config.bind_bot("old_bot".into(), false);
        config.allow_user(42);
        config.allow_workspace("/repo");
        config.active_conversation = Some("conversation-1".into());
        config.update_offset = 900;

        config.bind_bot("new_bot".into(), true);

        assert_eq!(config.bot_username.as_deref(), Some("new_bot"));
        assert!(config.allowed_user_ids.is_empty());
        assert_eq!(config.update_offset, 0);
        assert!(config.workspaces.is_empty());
        assert!(config.active_workspace.is_none());
        assert!(config.active_conversation.is_none());
        assert!(!config.is_linked());
    }

    #[test]
    fn workspaces_are_matched_exactly_and_never_resolved_from_a_message() {
        let mut config = TelegramConfig::default();
        config.allow_workspace("/repo");
        assert!(config.allows_workspace("/repo"));
        // Anything else is refused, so a chat message cannot reach a directory
        // that was never opted in.
        assert!(!config.allows_workspace("/repo/.."));
        assert!(!config.allows_workspace("/repo/sub"));
        assert!(!config.allows_workspace("/etc"));
        // The first allowed workspace becomes the active one.
        assert_eq!(config.active_workspace.as_deref(), Some("/repo"));
        config.allow_workspace("/other");
        assert_eq!(config.active_workspace.as_deref(), Some("/repo"));
    }

    #[test]
    fn token_shapes_are_checked_before_any_network_call() {
        assert!(looks_like_token(
            "8123456789:AAF-abcdefghijklmnopqrstuvwxyz12345"
        ));
        assert!(!looks_like_token("@argo_test_bot"));
        assert!(!looks_like_token("123:short"));
        assert!(!looks_like_token("notanumber:abcdefghijklmnopqrstuvwx"));
        assert!(!looks_like_token(""));
    }

    #[test]
    fn remove_deletes_both_files_and_reset_remains_an_idempotent_alias() {
        let (paths, _dir) = paths();
        save(&paths, &TelegramConfig::default()).expect("save");
        save_token(&paths, "123:abcdefghijklmnopqrstuvwx").expect("token");
        remove(&paths).expect("remove");
        assert_eq!(load(&paths).expect("load"), None);
        assert_eq!(load_token(&paths).expect("token"), None);
        reset(&paths).expect("legacy reset is a no-op when already removed");
    }
}
