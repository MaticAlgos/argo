//! Context epochs: explicit compaction points in a conversation.
//!
//! An epoch records that everything up to a message sequence has been folded into
//! a summary, so later turns can replay the summary instead of the messages. The
//! canonical rows are never deleted — an epoch only changes what gets *projected*
//! to an agent, which is what keeps `/compact` reversible and keeps the full
//! transcript readable in the TUI afterwards.
//!
//! Epochs are append-only. The newest row wins, and each one carries the previous
//! outline forward, so compacting twice cannot lose the first summary.

use crate::store::Store;
use argo_core::error::{ArgoError, Result};
use argo_core::ids::ConversationId;
use argo_core::now_millis;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// A recorded compaction point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEpoch {
    /// Stable id.
    pub id: String,
    /// Summary standing in for the compacted prefix, when one was produced.
    pub summary: Option<String>,
    /// Highest message sequence covered by the summary.
    ///
    /// Messages at or below this sequence are represented by `summary` rather
    /// than replayed verbatim.
    pub compacted_upto: i64,
    /// Why the epoch was created, for diagnostics: `manual` or `auto`.
    pub reason: String,
    /// Creation time in epoch millis.
    pub created_at: i64,
}

impl Store {
    /// Records a compaction point and returns the stored row.
    pub fn record_context_epoch(
        &self,
        conversation_id: &ConversationId,
        summary: Option<&str>,
        compacted_upto: i64,
        reason: &str,
    ) -> Result<ContextEpoch> {
        let epoch = ContextEpoch {
            id: uuid::Uuid::new_v4().to_string(),
            summary: summary.map(str::to_string),
            compacted_upto,
            reason: reason.to_string(),
            created_at: now_millis(),
        };
        self.conn
            .execute(
                "INSERT INTO context_epochs
                   (id, conversation_id, summary, compacted_upto, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    epoch.id,
                    conversation_id.as_str(),
                    epoch.summary,
                    epoch.compacted_upto,
                    epoch.reason,
                    epoch.created_at,
                ],
            )
            .map_err(|e| ArgoError::Store(format!("record context epoch: {e}")))?;
        Ok(epoch)
    }

    /// Records a compaction boundary and invalidates every native session in
    /// one transaction. Neither change is visible unless both commit.
    pub fn compact_context(
        &mut self,
        conversation_id: &ConversationId,
        summary: Option<&str>,
        compacted_upto: i64,
        reason: &str,
    ) -> Result<(ContextEpoch, usize)> {
        let epoch = ContextEpoch {
            id: uuid::Uuid::new_v4().to_string(),
            summary: summary.map(str::to_string),
            compacted_upto,
            reason: reason.to_string(),
            created_at: now_millis(),
        };
        let transaction = self
            .conn
            .transaction()
            .map_err(|e| ArgoError::Store(format!("begin context compaction: {e}")))?;
        transaction
            .execute(
                "INSERT INTO context_epochs
                   (id, conversation_id, summary, compacted_upto, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    epoch.id,
                    conversation_id.as_str(),
                    epoch.summary,
                    epoch.compacted_upto,
                    epoch.reason,
                    epoch.created_at,
                ],
            )
            .map_err(|e| ArgoError::Store(format!("record context epoch: {e}")))?;
        let sessions_cleared = transaction
            .execute(
                "DELETE FROM agent_sessions WHERE conversation_id = ?1",
                rusqlite::params![conversation_id.as_str()],
            )
            .map_err(|e| ArgoError::Store(format!("clear agent sessions: {e}")))?;
        transaction
            .commit()
            .map_err(|e| ArgoError::Store(format!("commit context compaction: {e}")))?;
        Ok((epoch, sessions_cleared))
    }

    /// Loads the newest compaction point, if the conversation has one.
    ///
    /// Ordered by sequence rather than time so two epochs written in the same
    /// millisecond still resolve deterministically to the wider one.
    pub fn latest_context_epoch(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<ContextEpoch>> {
        self.conn
            .query_row(
                "SELECT id, summary, compacted_upto, reason, created_at
                   FROM context_epochs
                  WHERE conversation_id = ?1
                  ORDER BY compacted_upto DESC, created_at DESC
                  LIMIT 1",
                rusqlite::params![conversation_id.as_str()],
                |row| {
                    Ok(ContextEpoch {
                        id: row.get(0)?,
                        summary: row.get(1)?,
                        compacted_upto: row.get(2)?,
                        reason: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("latest context epoch: {e}")))
    }

    /// Highest message sequence currently stored for a conversation.
    ///
    /// This is the boundary `/compact` folds up to. Returns `None` for an empty
    /// conversation, which is what lets the caller refuse to compact nothing.
    pub fn max_message_seq(&self, conversation_id: &ConversationId) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MAX(seq) FROM messages WHERE conversation_id = ?1",
                rusqlite::params![conversation_id.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(|e| ArgoError::Store(format!("max message seq: {e}")))
    }

    /// Removes every stored native session handle for a conversation.
    ///
    /// Compaction is meaningless while a vendor CLI still holds the full history
    /// in its own session: the reduced projection would never be sent. Dropping
    /// the handles forces the next turn down the fresh-with-context path.
    ///
    /// Returns how many handles were dropped.
    pub fn clear_all_agent_sessions(&self, conversation_id: &ConversationId) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM agent_sessions WHERE conversation_id = ?1",
                rusqlite::params![conversation_id.as_str()],
            )
            .map_err(|e| ArgoError::Store(format!("clear agent sessions: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::NewMessage;
    use argo_core::ids::{AgentId, SessionId};
    use argo_core::session::AgentSessionRecord;

    fn setup() -> (Store, ConversationId) {
        let store = Store::open_in_memory().expect("store");
        let ws = store.ensure_workspace(std::env::temp_dir()).expect("ws");
        let conv = store.create_conversation(&ws, None).expect("conv");
        (store, conv)
    }

    #[test]
    fn a_conversation_starts_with_no_epoch() {
        let (s, conv) = setup();
        assert!(s.latest_context_epoch(&conv).expect("query").is_none());
        assert!(s.max_message_seq(&conv).expect("seq").is_none());
    }

    #[test]
    fn the_newest_epoch_wins() {
        let (s, conv) = setup();
        s.record_context_epoch(&conv, Some("first"), 4, "manual")
            .expect("first");
        s.record_context_epoch(&conv, Some("second"), 9, "manual")
            .expect("second");
        let latest = s.latest_context_epoch(&conv).expect("query").expect("some");
        assert_eq!(latest.compacted_upto, 9);
        assert_eq!(latest.summary.as_deref(), Some("second"));
    }

    #[test]
    fn max_seq_tracks_appended_messages() {
        let (s, conv) = setup();
        s.append_message(&conv, NewMessage::user("one")).expect("a");
        s.append_message(&conv, NewMessage::user("two")).expect("b");
        assert_eq!(s.max_message_seq(&conv).expect("seq"), Some(2));
    }

    fn add_sessions(store: &Store, conversation_id: &ConversationId) {
        for agent in ["claude", "codex", "kiro"] {
            store
                .upsert_agent_session(
                    conversation_id,
                    &AgentSessionRecord {
                        agent_id: AgentId::new(agent),
                        session_id: SessionId::new(format!("sess-{agent}")),
                        model: None,
                        cwd: None,
                        stable_hash: None,
                        last_message_id: None,
                        updated_at: 0,
                    },
                )
                .expect("upsert");
        }
    }

    #[test]
    fn compaction_records_the_epoch_and_drops_every_native_handle_atomically() {
        // One agent left holding a session would keep replying from uncompacted
        // history, which is exactly the inconsistency /compact must avoid.
        let (mut s, conv) = setup();
        add_sessions(&s, &conv);
        let (epoch, cleared) = s
            .compact_context(&conv, Some("summary"), 7, "manual")
            .expect("compact");
        assert_eq!(cleared, 3);
        assert_eq!(epoch.compacted_upto, 7);
        assert!(s.list_agent_sessions(&conv).expect("list").is_empty());
        assert_eq!(
            s.latest_context_epoch(&conv)
                .expect("epoch")
                .expect("recorded")
                .summary
                .as_deref(),
            Some("summary")
        );
    }

    #[test]
    fn compaction_rolls_back_the_epoch_when_session_invalidation_fails() {
        let (mut s, conv) = setup();
        add_sessions(&s, &conv);
        s.conn
            .execute_batch(
                "CREATE TRIGGER reject_session_clear
                 BEFORE DELETE ON agent_sessions
                 BEGIN SELECT RAISE(ABORT, 'injected clear failure'); END;",
            )
            .expect("trigger");

        let error = s
            .compact_context(&conv, Some("must roll back"), 7, "manual")
            .expect_err("clear must fail");
        assert!(error.to_string().contains("injected clear failure"));
        assert!(s.latest_context_epoch(&conv).expect("epoch").is_none());
        assert_eq!(s.list_agent_sessions(&conv).expect("sessions").len(), 3);
    }

    #[test]
    fn epochs_are_scoped_to_their_conversation() {
        let (s, first) = setup();
        let ws = s.ensure_workspace(std::env::temp_dir()).expect("ws");
        let second = s.create_conversation(&ws, None).expect("second");
        s.record_context_epoch(&first, Some("only first"), 3, "manual")
            .expect("record");
        assert!(s.latest_context_epoch(&second).expect("query").is_none());
    }
}
