//! Native session handles and the resume cursor.
//!
//! One row per `(conversation, agent)` holds the upstream CLI's own session
//! handle plus the identity it was created under. Together with
//! [`Store::latest_completed_assistant_message_id`] this supplies every input
//! the resume guard needs.

use crate::store::Store;
use argo_core::error::{ArgoError, Result};
use argo_core::ids::{AgentId, ConversationId, MessageId, SessionId};
use argo_core::now_millis;
use argo_core::session::AgentSessionRecord;
use rusqlite::OptionalExtension;

impl Store {
    /// Stores or replaces the handle for one `(conversation, agent)` pair.
    ///
    /// Callers must pass the model, cwd, and cursor in force when the session was
    /// created; omitting them leaves an unverifiable row that the guard treats as
    /// `MissingCursor` and reseeds on every turn.
    pub fn upsert_agent_session(
        &self,
        conversation_id: &ConversationId,
        record: &AgentSessionRecord,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO agent_sessions
                   (conversation_id, agent_id, session_id, model, cwd, stable_hash,
                    last_message_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(conversation_id, agent_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    model = excluded.model,
                    cwd = excluded.cwd,
                    stable_hash = excluded.stable_hash,
                    last_message_id = excluded.last_message_id,
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    conversation_id.as_str(),
                    record.agent_id.as_str(),
                    record.session_id.as_str(),
                    record.model,
                    record.cwd,
                    record.stable_hash,
                    record.last_message_id.as_ref().map(|m| m.to_string()),
                    now_millis(),
                ],
            )
            .map_err(|e| ArgoError::Store(format!("upsert agent session: {e}")))?;
        Ok(())
    }

    /// Loads the stored handle for one agent, if any.
    pub fn get_agent_session(
        &self,
        conversation_id: &ConversationId,
        agent_id: &AgentId,
    ) -> Result<Option<AgentSessionRecord>> {
        self.conn
            .query_row(
                "SELECT agent_id, session_id, model, cwd, stable_hash, last_message_id, updated_at
                   FROM agent_sessions WHERE conversation_id = ?1 AND agent_id = ?2",
                rusqlite::params![conversation_id.as_str(), agent_id.as_str()],
                |row| {
                    Ok(AgentSessionRecord {
                        agent_id: AgentId::new(row.get::<_, String>(0)?),
                        session_id: SessionId::new(row.get::<_, String>(1)?),
                        model: row.get(2)?,
                        cwd: row.get(3)?,
                        stable_hash: row.get(4)?,
                        last_message_id: row.get::<_, Option<String>>(5)?.map(MessageId::new),
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("get agent session: {e}")))
    }

    /// Removes a stored handle.
    ///
    /// Called when a resume attempt proves the upstream session is gone, so the
    /// next turn starts fresh instead of retrying a dead handle forever.
    pub fn clear_agent_session(
        &self,
        conversation_id: &ConversationId,
        agent_id: &AgentId,
    ) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM agent_sessions WHERE conversation_id = ?1 AND agent_id = ?2",
                rusqlite::params![conversation_id.as_str(), agent_id.as_str()],
            )
            .map_err(|e| ArgoError::Store(format!("clear agent session: {e}")))?;
        Ok(())
    }

    /// Lists every agent that holds a live handle in this conversation.
    pub fn list_agent_sessions(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<AgentSessionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT agent_id, session_id, model, cwd, stable_hash, last_message_id, updated_at
                   FROM agent_sessions WHERE conversation_id = ?1 ORDER BY agent_id",
            )
            .map_err(|e| ArgoError::Store(format!("prepare list sessions: {e}")))?;
        let rows = stmt
            .query_map([conversation_id.as_str()], |row| {
                Ok(AgentSessionRecord {
                    agent_id: AgentId::new(row.get::<_, String>(0)?),
                    session_id: SessionId::new(row.get::<_, String>(1)?),
                    model: row.get(2)?,
                    cwd: row.get(3)?,
                    stable_hash: row.get(4)?,
                    last_message_id: row.get::<_, Option<String>>(5)?.map(MessageId::new),
                    updated_at: row.get(6)?,
                })
            })
            .map_err(|e| ArgoError::Store(format!("list sessions: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ArgoError::Store(format!("read sessions: {e}")))
    }

    /// Newest assistant message that counts as a completed turn.
    ///
    /// Two exclusions make this correct, and both were learned from OpenDesign:
    ///
    /// - `exclude` drops the current turn's in-flight placeholder, which is the
    ///   newest row by position but has no terminal status yet.
    /// - `admit` re-admits the stored session's own cursor, so a session whose
    ///   last turn failed but which is still resumable continues to match its
    ///   cursor instead of being needlessly reseeded.
    ///
    /// Failed and cancelled runs never advance the cursor, so an unrelated failed
    /// turn by another agent does not force a cold reseed either.
    pub fn latest_completed_assistant_message_id(
        &self,
        conversation_id: &ConversationId,
        exclude: Option<&MessageId>,
        admit: Option<&MessageId>,
    ) -> Result<Option<MessageId>> {
        let exclude = exclude.map(|m| m.to_string()).unwrap_or_default();
        let admit = admit.map(|m| m.to_string()).unwrap_or_default();

        self.conn
            .query_row(
                "SELECT m.id
                   FROM messages m
                   LEFT JOIN runs r ON r.id = m.run_id
                  WHERE m.conversation_id = ?1
                    AND m.role = 'assistant'
                    AND m.id <> ?2
                    AND (r.status = 'succeeded' OR m.id = ?3)
                  ORDER BY m.seq DESC
                  LIMIT 1",
                rusqlite::params![conversation_id.as_str(), exclude, admit],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("latest completed assistant: {e}")))
            .map(|opt| opt.map(MessageId::new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::NewMessage;
    use crate::runs::NewRun;
    use argo_core::event::RunStatus;
    use argo_core::ids::WorkspaceId;
    use argo_core::message::ContentBlock;

    fn setup() -> (Store, ConversationId, WorkspaceId) {
        let store = Store::open_in_memory().expect("store");
        let ws = store.ensure_workspace(std::env::temp_dir()).expect("ws");
        let conv = store.create_conversation(&ws, None).expect("conv");
        (store, conv, ws)
    }

    fn record(agent: &str, session: &str, cursor: Option<&str>) -> AgentSessionRecord {
        AgentSessionRecord {
            agent_id: AgentId::new(agent),
            session_id: SessionId::new(session),
            model: Some("m1".into()),
            cwd: Some("/repo".into()),
            stable_hash: Some("h1".into()),
            last_message_id: cursor.map(MessageId::new),
            updated_at: 0,
        }
    }

    /// Appends a completed assistant turn and returns its message id.
    fn completed_assistant(
        store: &Store,
        conv: &ConversationId,
        ws: &WorkspaceId,
        agent: &str,
        status: RunStatus,
    ) -> MessageId {
        let run = store
            .create_run(NewRun {
                conversation_id: conv.clone(),
                workspace_id: ws.clone(),
                agent_id: AgentId::new(agent),
                model: None,
                resumed: false,
                invalidation_reason: None,
                parent_run_id: None,
            })
            .expect("run");
        let msg = store
            .append_message(
                conv,
                NewMessage::assistant(
                    vec![ContentBlock::text("reply")],
                    AgentId::new(agent),
                    None,
                    run.clone(),
                ),
            )
            .expect("message");
        store.attach_run_message(&run, &msg).expect("attach");
        store.finish_run(&run, status, None).expect("finish");
        msg
    }

    #[test]
    fn sessions_are_scoped_per_agent() {
        // A conversation can hold live handles on several CLIs simultaneously;
        // that is what makes switching back cheap when nothing else changed.
        let (s, conv, _) = setup();
        s.upsert_agent_session(&conv, &record("claude", "sess-A", None))
            .expect("claude");
        s.upsert_agent_session(&conv, &record("codex", "thread-B", None))
            .expect("codex");

        let claude = s
            .get_agent_session(&conv, &AgentId::new("claude"))
            .expect("get")
            .expect("some");
        assert_eq!(claude.session_id, SessionId::new("sess-A"));
        let codex = s
            .get_agent_session(&conv, &AgentId::new("codex"))
            .expect("get")
            .expect("some");
        assert_eq!(codex.session_id, SessionId::new("thread-B"));
        assert_eq!(s.list_agent_sessions(&conv).expect("list").len(), 2);
    }

    #[test]
    fn upsert_replaces_the_handle_for_the_same_agent() {
        let (s, conv, _) = setup();
        s.upsert_agent_session(&conv, &record("claude", "old", None))
            .expect("first");
        s.upsert_agent_session(&conv, &record("claude", "new", None))
            .expect("second");
        assert_eq!(
            s.get_agent_session(&conv, &AgentId::new("claude"))
                .expect("get")
                .expect("some")
                .session_id,
            SessionId::new("new")
        );
    }

    #[test]
    fn clearing_removes_only_the_targeted_agent() {
        let (s, conv, _) = setup();
        s.upsert_agent_session(&conv, &record("claude", "a", None))
            .expect("a");
        s.upsert_agent_session(&conv, &record("kiro", "b", None))
            .expect("b");
        s.clear_agent_session(&conv, &AgentId::new("claude"))
            .expect("clear");
        assert!(s
            .get_agent_session(&conv, &AgentId::new("claude"))
            .expect("get")
            .is_none());
        assert!(s
            .get_agent_session(&conv, &AgentId::new("kiro"))
            .expect("get")
            .is_some());
    }

    #[test]
    fn absent_session_reads_as_none() {
        let (s, conv, _) = setup();
        assert!(s
            .get_agent_session(&conv, &AgentId::new("grok"))
            .expect("get")
            .is_none());
    }

    #[test]
    fn cursor_is_none_before_any_completed_turn() {
        let (s, conv, _) = setup();
        s.append_message(&conv, NewMessage::user("hi"))
            .expect("user");
        assert!(s
            .latest_completed_assistant_message_id(&conv, None, None)
            .expect("query")
            .is_none());
    }

    #[test]
    fn cursor_tracks_the_newest_succeeded_assistant_turn() {
        let (s, conv, ws) = setup();
        let first = completed_assistant(&s, &conv, &ws, "claude", RunStatus::Succeeded);
        assert_eq!(
            s.latest_completed_assistant_message_id(&conv, None, None)
                .expect("query"),
            Some(first)
        );
        let second = completed_assistant(&s, &conv, &ws, "codex", RunStatus::Succeeded);
        assert_eq!(
            s.latest_completed_assistant_message_id(&conv, None, None)
                .expect("query"),
            Some(second)
        );
    }

    #[test]
    fn failed_and_cancelled_turns_do_not_advance_the_cursor() {
        // Otherwise an unrelated crash by another agent would invalidate a
        // perfectly good session and force a full transcript replay.
        let (s, conv, ws) = setup();
        let good = completed_assistant(&s, &conv, &ws, "claude", RunStatus::Succeeded);
        completed_assistant(&s, &conv, &ws, "codex", RunStatus::Failed);
        completed_assistant(&s, &conv, &ws, "grok", RunStatus::Cancelled);
        assert_eq!(
            s.latest_completed_assistant_message_id(&conv, None, None)
                .expect("query"),
            Some(good)
        );
    }

    #[test]
    fn in_flight_placeholder_is_excluded() {
        let (s, conv, ws) = setup();
        let good = completed_assistant(&s, &conv, &ws, "claude", RunStatus::Succeeded);
        // Pin a placeholder for the turn currently being submitted.
        let placeholder = MessageId::new("in-flight");
        s.append_message_with_id(
            &conv,
            placeholder.clone(),
            NewMessage::assistant(vec![], AgentId::new("claude"), None, RunId::new("pending")),
        )
        .expect("pin");
        assert_eq!(
            s.latest_completed_assistant_message_id(&conv, Some(&placeholder), None)
                .expect("query"),
            Some(good)
        );
    }

    #[test]
    fn admitted_cursor_survives_its_own_failed_turn() {
        // A resumable session whose last turn failed must still match its stored
        // cursor, or every retry would pay a cold reseed.
        let (s, conv, ws) = setup();
        let failed = completed_assistant(&s, &conv, &ws, "claude", RunStatus::Failed);
        assert!(s
            .latest_completed_assistant_message_id(&conv, None, None)
            .expect("query")
            .is_none());
        assert_eq!(
            s.latest_completed_assistant_message_id(&conv, None, Some(&failed))
                .expect("query"),
            Some(failed)
        );
    }

    use argo_core::ids::RunId;
}
