//! Store handle, workspaces, and conversations.

use argo_core::error::{ArgoError, Result};
use argo_core::ids::{ConversationId, RunId, WorkspaceId};
use argo_core::now_millis;
use argo_core::session::SelectionChange;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A conversation row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    /// Stable id.
    pub id: ConversationId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Display title, when set.
    pub title: Option<String>,
    /// Parent conversation for delegated child sessions.
    pub parent_conversation_id: Option<ConversationId>,
    /// Parent run that spawned this conversation.
    pub parent_run_id: Option<RunId>,
    /// Agent selected for the next turn.
    pub selected_agent_id: Option<String>,
    /// Model selected for the next turn.
    pub selected_model: Option<String>,
    /// Reasoning effort selected for the next turn.
    pub selected_reasoning: Option<String>,
    /// Execution mode selected for the next turn.
    pub selected_mode: Option<String>,
    /// Creation time in epoch millis.
    pub created_at: i64,
    /// Last update time in epoch millis.
    pub updated_at: i64,
}

/// Canonical Argo store.
///
/// Holds a single connection; the daemon owns one `Store` and serializes writes
/// through it so multiple terminals cannot interleave partial turns.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and migrates it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| ArgoError::Store(format!("open {}: {e}", path.display())))?;
        crate::schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Opens an in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()
            .map_err(|e| ArgoError::Store(format!("open memory: {e}")))?;
        crate::schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Verifies the database is readable and structurally intact.
    pub fn verify(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| ArgoError::Store(format!("integrity_check: {e}")))?;
        if result != "ok" {
            return Err(ArgoError::Store(format!(
                "integrity check failed: {result}"
            )));
        }
        Ok(())
    }

    /// Returns the workspace for `root`, creating it on first use.
    ///
    /// The root is stored canonicalized so the same project reached through a
    /// symlink does not create a second workspace — which would split history
    /// and break the resume guard's cwd comparison.
    pub fn ensure_workspace(&self, root: impl AsRef<Path>) -> Result<WorkspaceId> {
        let canonical = std::fs::canonicalize(root.as_ref())
            .unwrap_or_else(|_| root.as_ref().to_path_buf())
            .to_string_lossy()
            .to_string();

        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM workspaces WHERE root = ?1",
                [&canonical],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("lookup workspace: {e}")))?
        {
            return Ok(WorkspaceId::new(id));
        }

        let id = WorkspaceId::generate();
        self.conn
            .execute(
                "INSERT INTO workspaces (id, root, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![id.as_str(), canonical, now_millis()],
            )
            .map_err(|e| ArgoError::Store(format!("insert workspace: {e}")))?;
        Ok(id)
    }

    /// Returns the canonical root for a workspace.
    pub fn workspace_root(&self, id: &WorkspaceId) -> Result<String> {
        self.conn
            .query_row(
                "SELECT root FROM workspaces WHERE id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("lookup workspace root: {e}")))?
            .ok_or_else(|| ArgoError::not_found("workspace", id.as_str()))
    }

    /// Creates a conversation.
    pub fn create_conversation(
        &self,
        workspace_id: &WorkspaceId,
        title: Option<&str>,
    ) -> Result<ConversationId> {
        let id = ConversationId::generate();
        let now = now_millis();
        self.conn
            .execute(
                "INSERT INTO conversations (id, workspace_id, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![id.as_str(), workspace_id.as_str(), title, now],
            )
            .map_err(|e| ArgoError::Store(format!("insert conversation: {e}")))?;
        Ok(id)
    }

    /// Creates a child conversation linked to the run that spawned it.
    pub fn create_child_conversation(
        &self,
        workspace_id: &WorkspaceId,
        parent_conversation_id: &ConversationId,
        parent_run_id: Option<&RunId>,
        title: Option<&str>,
    ) -> Result<ConversationId> {
        let id = ConversationId::generate();
        let now = now_millis();
        self.conn
            .execute(
                "INSERT INTO conversations
                   (id, workspace_id, title, parent_conversation_id, parent_run_id,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                rusqlite::params![
                    id.as_str(),
                    workspace_id.as_str(),
                    title,
                    parent_conversation_id.as_str(),
                    parent_run_id.as_ref().map(|run| run.as_str()),
                    now
                ],
            )
            .map_err(|e| ArgoError::Store(format!("insert child conversation: {e}")))?;
        Ok(id)
    }

    /// Loads a conversation.
    pub fn get_conversation(&self, id: &ConversationId) -> Result<Conversation> {
        self.conn
            .query_row(
                "SELECT id, workspace_id, title, parent_conversation_id, parent_run_id,
                        selected_agent_id, selected_model, selected_reasoning,
                        selected_mode, created_at, updated_at
                   FROM conversations WHERE id = ?1",
                [id.as_str()],
                map_conversation,
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("get conversation: {e}")))?
            .ok_or_else(|| ArgoError::not_found("conversation", id.as_str()))
    }

    /// Lists conversations in a workspace, newest activity first.
    pub fn list_conversations(&self, workspace_id: &WorkspaceId) -> Result<Vec<Conversation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, workspace_id, title, parent_conversation_id, parent_run_id,
                        selected_agent_id, selected_model, selected_reasoning,
                        selected_mode, created_at, updated_at
                   FROM conversations
                  WHERE workspace_id = ?1
                  ORDER BY updated_at DESC, rowid DESC",
            )
            .map_err(|e| ArgoError::Store(format!("prepare list conversations: {e}")))?;
        let rows = stmt
            .query_map([workspace_id.as_str()], map_conversation)
            .map_err(|e| ArgoError::Store(format!("list conversations: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ArgoError::Store(format!("read conversations: {e}")))
    }

    /// Lists direct child conversations of a conversation.
    pub fn list_child_conversations(&self, id: &ConversationId) -> Result<Vec<Conversation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, workspace_id, title, parent_conversation_id, parent_run_id,
                        selected_agent_id, selected_model, selected_reasoning,
                        selected_mode, created_at, updated_at
                   FROM conversations
                  WHERE parent_conversation_id = ?1
                  ORDER BY created_at ASC",
            )
            .map_err(|e| ArgoError::Store(format!("prepare list children: {e}")))?;
        let rows = stmt
            .query_map([id.as_str()], map_conversation)
            .map_err(|e| ArgoError::Store(format!("list children: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ArgoError::Store(format!("read children: {e}")))
    }

    /// Records a pending agent/model/reasoning selection.
    ///
    /// Applied at the next turn boundary. Only the fields present in `change` are
    /// written, so `/model` does not clear a previously chosen reasoning level.
    pub fn update_selection(&self, id: &ConversationId, change: &SelectionChange) -> Result<()> {
        if change.is_empty() {
            return Ok(());
        }
        let existing = self.get_conversation(id)?;
        let agent = change
            .agent_id
            .as_ref()
            .map(|a| a.to_string())
            .or(existing.selected_agent_id);
        // Switching agent without naming a model must not carry the previous
        // agent's model id across: model ids are not portable between CLIs.
        let model = match (&change.agent_id, &change.model) {
            (_, Some(m)) => Some(m.clone()),
            (Some(_), None) => None,
            (None, None) => existing.selected_model,
        };
        let reasoning = match (&change.agent_id, &change.reasoning) {
            (_, Some(r)) => Some(r.clone()),
            (Some(_), None) => None,
            (None, None) => existing.selected_reasoning,
        };

        self.conn
            .execute(
                "UPDATE conversations
                    SET selected_agent_id = ?2,
                        selected_model = ?3,
                        selected_reasoning = ?4,
                        updated_at = ?5
                  WHERE id = ?1",
                rusqlite::params![id.as_str(), agent, model, reasoning, now_millis()],
            )
            .map_err(|e| ArgoError::Store(format!("update selection: {e}")))?;
        Ok(())
    }

    /// Records the execution mode applied at the next turn.
    pub fn set_mode(&self, id: &ConversationId, mode: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE conversations SET selected_mode = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id.as_str(), mode, now_millis()],
            )
            .map_err(|e| ArgoError::Store(format!("set mode: {e}")))?;
        Ok(())
    }

    /// Sets a conversation title.
    pub fn set_title(&self, id: &ConversationId, title: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id.as_str(), title, now_millis()],
            )
            .map_err(|e| ArgoError::Store(format!("set title: {e}")))?;
        Ok(())
    }

    /// Generates titles for older conversations created before automatic naming.
    ///
    /// Empty conversations stay untitled; the first meaningful user request is
    /// used for everything else. Explicit titles are never changed.
    pub fn backfill_missing_titles(&self) -> Result<usize> {
        let ids: Vec<ConversationId> = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT id FROM conversations
                     WHERE title IS NULL OR trim(title) = ''
                     ORDER BY created_at ASC",
                )
                .map_err(|e| ArgoError::Store(format!("prepare title backfill: {e}")))?;
            let rows = statement
                .query_map([], |row| Ok(ConversationId::new(row.get::<_, String>(0)?)))
                .map_err(|e| ArgoError::Store(format!("query title backfill: {e}")))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| ArgoError::Store(format!("read title backfill: {e}")))?
        };

        let mut updated = 0usize;
        for id in ids {
            let prompt = self
                .list_messages(&id)?
                .into_iter()
                .find(|message| message.role == argo_core::message::Role::User)
                .map(|message| message.transferable_text())
                .filter(|text| !text.trim().is_empty());
            if let Some(prompt) = prompt {
                self.set_title(&id, &argo_core::conversation_title(&prompt))?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Marks a conversation as recently active.
    pub(crate) fn touch_conversation(&self, id: &ConversationId) -> Result<()> {
        self.conn
            .execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                rusqlite::params![id.as_str(), now_millis()],
            )
            .map_err(|e| ArgoError::Store(format!("touch conversation: {e}")))?;
        Ok(())
    }

    /// Fails when `conversation` does not belong to `workspace`.
    ///
    /// Guards the cross-workspace mixup OpenDesign hit: a run whose cwd came from
    /// one project but whose rows landed in another project's conversation,
    /// corrupting both the history and the resume identity.
    pub fn assert_conversation_in_workspace(
        &self,
        conversation: &ConversationId,
        workspace: &WorkspaceId,
    ) -> Result<()> {
        let owner = self.get_conversation(conversation)?.workspace_id;
        if &owner != workspace {
            return Err(ArgoError::Invalid(format!(
                "conversation {conversation} belongs to workspace {owner}, not {workspace}"
            )));
        }
        Ok(())
    }
}

fn map_conversation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: ConversationId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        title: row.get(2)?,
        parent_conversation_id: row.get::<_, Option<String>>(3)?.map(ConversationId::new),
        parent_run_id: row.get::<_, Option<String>>(4)?.map(RunId::new),
        selected_agent_id: row.get(5)?,
        selected_model: row.get(6)?,
        selected_reasoning: row.get(7)?,
        selected_mode: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::ids::AgentId;

    fn store() -> Store {
        Store::open_in_memory().expect("store")
    }

    #[test]
    fn integrity_check_passes_on_a_fresh_store() {
        store().verify().expect("verify");
    }

    #[test]
    fn workspaces_are_deduplicated_by_root() {
        let s = store();
        let tmp = std::env::temp_dir();
        let a = s.ensure_workspace(&tmp).expect("a");
        let b = s.ensure_workspace(&tmp).expect("b");
        assert_eq!(a, b);
    }

    #[test]
    fn conversations_round_trip() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let id = s.create_conversation(&ws, Some("first")).expect("create");
        let loaded = s.get_conversation(&id).expect("load");
        assert_eq!(loaded.title.as_deref(), Some("first"));
        assert_eq!(loaded.workspace_id, ws);
        assert!(loaded.parent_conversation_id.is_none());
    }

    #[test]
    fn the_execution_mode_persists() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let id = s.create_conversation(&ws, None).expect("create");
        assert!(s
            .get_conversation(&id)
            .expect("load")
            .selected_mode
            .is_none());

        s.set_mode(&id, Some("plan")).expect("set");
        assert_eq!(
            s.get_conversation(&id)
                .expect("load")
                .selected_mode
                .as_deref(),
            Some("plan")
        );
        // Clearing returns to the default.
        s.set_mode(&id, None).expect("clear");
        assert!(s
            .get_conversation(&id)
            .expect("load")
            .selected_mode
            .is_none());
    }

    #[test]
    fn missing_conversation_is_not_found() {
        let s = store();
        let err = s
            .get_conversation(&ConversationId::new("nope"))
            .expect_err("must fail");
        assert_eq!(err.code(), "NOT_FOUND");
    }

    #[test]
    fn switching_agent_clears_the_previous_model_selection() {
        // Model ids are not portable across CLIs: carrying `sonnet` into Codex
        // would produce an invalid invocation.
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let id = s.create_conversation(&ws, None).expect("create");

        s.update_selection(
            &id,
            &SelectionChange {
                agent_id: Some(AgentId::new("claude")),
                model: Some("sonnet".into()),
                reasoning: None,
            },
        )
        .expect("select claude");
        assert_eq!(
            s.get_conversation(&id)
                .expect("load")
                .selected_model
                .as_deref(),
            Some("sonnet")
        );

        s.update_selection(
            &id,
            &SelectionChange {
                agent_id: Some(AgentId::new("codex")),
                ..Default::default()
            },
        )
        .expect("switch to codex");
        let after = s.get_conversation(&id).expect("load");
        assert_eq!(after.selected_agent_id.as_deref(), Some("codex"));
        assert_eq!(after.selected_model, None);
    }

    #[test]
    fn changing_only_the_model_keeps_the_agent_and_reasoning() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let id = s.create_conversation(&ws, None).expect("create");
        s.update_selection(
            &id,
            &SelectionChange {
                agent_id: Some(AgentId::new("grok")),
                model: Some("grok-4.3".into()),
                reasoning: Some("high".into()),
            },
        )
        .expect("initial");
        s.update_selection(
            &id,
            &SelectionChange {
                model: Some("grok-4.20-reasoning".into()),
                ..Default::default()
            },
        )
        .expect("model only");
        let after = s.get_conversation(&id).expect("load");
        assert_eq!(after.selected_agent_id.as_deref(), Some("grok"));
        assert_eq!(after.selected_model.as_deref(), Some("grok-4.20-reasoning"));
        assert_eq!(after.selected_reasoning.as_deref(), Some("high"));
    }

    #[test]
    fn empty_selection_change_is_a_noop() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let id = s.create_conversation(&ws, None).expect("create");
        s.update_selection(&id, &SelectionChange::default())
            .expect("noop");
        assert!(s
            .get_conversation(&id)
            .expect("load")
            .selected_agent_id
            .is_none());
    }

    #[test]
    fn cross_workspace_conversation_use_is_rejected() {
        let s = store();
        let ws_a = s.ensure_workspace(std::env::temp_dir()).expect("a");
        let other = std::env::temp_dir().join("argo-ws-b");
        std::fs::create_dir_all(&other).expect("mkdir");
        let ws_b = s.ensure_workspace(&other).expect("b");
        let conv = s.create_conversation(&ws_a, None).expect("conv");

        s.assert_conversation_in_workspace(&conv, &ws_a)
            .expect("ok");
        let err = s
            .assert_conversation_in_workspace(&conv, &ws_b)
            .expect_err("must reject");
        assert_eq!(err.code(), "INVALID_REQUEST");
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn child_conversations_are_linked_and_listable() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let parent = s.create_conversation(&ws, Some("parent")).expect("parent");
        let run = RunId::new("run-1");
        let child = s
            .create_child_conversation(&ws, &parent, Some(&run), Some("codex review"))
            .expect("child");

        let children = s.list_child_conversations(&parent).expect("list");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child);
        assert_eq!(children[0].parent_run_id, Some(run));
        assert!(s.list_child_conversations(&child).expect("none").is_empty());
    }

    #[test]
    fn list_conversations_orders_by_recent_activity() {
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let first = s.create_conversation(&ws, Some("older")).expect("a");
        let second = s.create_conversation(&ws, Some("newer")).expect("b");
        // Timestamps are epoch millis, so two writes inside one millisecond would
        // tie; wait long enough that the ordering under test is the real signal.
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.touch_conversation(&first).expect("touch");
        let list = s.list_conversations(&ws).expect("list");
        assert_eq!(list.len(), 2);
        // `first` was touched last, so it sorts first.
        assert_eq!(list[0].id, first);
        assert_eq!(list[1].id, second);
    }

    #[test]
    fn conversation_ordering_is_stable_when_timestamps_tie() {
        // Ties are possible on fast machines; ordering must still be total so the
        // TUI list does not shuffle between refreshes.
        let s = store();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        for i in 0..5 {
            s.create_conversation(&ws, Some(&format!("c{i}")))
                .expect("create");
        }
        let first = s.list_conversations(&ws).expect("list");
        let second = s.list_conversations(&ws).expect("list");
        assert_eq!(
            first.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            second.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
    }
}
