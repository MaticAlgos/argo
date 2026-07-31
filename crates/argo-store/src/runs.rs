//! Run and event persistence.
//!
//! A run is one agent invocation. Events are appended with dense per-run
//! sequence numbers so a reconnecting client can replay everything after the
//! last sequence it saw, rather than re-reading the whole turn.

use crate::store::Store;
use argo_core::error::{ArgoError, Result};
use argo_core::event::{EventSeq, RunEvent, RunEventKind, RunStatus};
use argo_core::ids::{AgentId, ConversationId, MessageId, RunId, WorkspaceId};
use argo_core::now_millis;
use argo_core::session::InvalidationReason;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// Fields needed to create a run.
#[derive(Debug, Clone)]
pub struct NewRun {
    /// Conversation the run belongs to.
    pub conversation_id: ConversationId,
    /// Workspace the run executes in.
    pub workspace_id: WorkspaceId,
    /// Adapter handling the run.
    pub agent_id: AgentId,
    /// Resolved model.
    pub model: Option<String>,
    /// True when the upstream session was resumed rather than created.
    pub resumed: bool,
    /// Why a stored session was rejected, when it was.
    pub invalidation_reason: Option<InvalidationReason>,
    /// Parent run for delegated child runs.
    pub parent_run_id: Option<RunId>,
}

/// A run row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// Stable id.
    pub id: RunId,
    /// Owning conversation.
    pub conversation_id: ConversationId,
    /// Executing workspace.
    pub workspace_id: WorkspaceId,
    /// Adapter that handled the run.
    pub agent_id: AgentId,
    /// Resolved model.
    pub model: Option<String>,
    /// Current status.
    pub status: RunStatus,
    /// Assistant message this run produced.
    pub assistant_message_id: Option<MessageId>,
    /// Parent run, for delegated children.
    pub parent_run_id: Option<RunId>,
    /// True when the upstream session was resumed.
    pub resumed: bool,
    /// Why a stored session was rejected, when it was.
    pub invalidation_reason: Option<String>,
    /// Stable error code when the run failed.
    pub error_code: Option<String>,
    /// Error detail when the run failed.
    pub error_message: Option<String>,
    /// Creation time in epoch millis.
    pub created_at: i64,
    /// Completion time in epoch millis.
    pub finished_at: Option<i64>,
}

fn status_to_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(value: &str) -> Result<RunStatus> {
    match value {
        "pending" => Ok(RunStatus::Pending),
        "running" => Ok(RunStatus::Running),
        "succeeded" => Ok(RunStatus::Succeeded),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        other => Err(ArgoError::Store(format!("unknown run status: {other}"))),
    }
}

fn reason_to_str(reason: InvalidationReason) -> &'static str {
    match reason {
        InvalidationReason::ModelChanged => "model_changed",
        InvalidationReason::CwdChanged => "cwd_changed",
        InvalidationReason::ConversationAdvanced => "conversation_advanced",
        InvalidationReason::MissingCursor => "missing_cursor",
        InvalidationReason::Unsupported => "unsupported",
    }
}

impl Store {
    /// Creates a run in `Pending` state.
    ///
    /// Rejects a conversation that belongs to a different workspace, which would
    /// otherwise execute against one project's files while writing into another
    /// project's history.
    pub fn create_run(&self, new_run: NewRun) -> Result<RunId> {
        self.assert_conversation_in_workspace(&new_run.conversation_id, &new_run.workspace_id)?;
        let id = RunId::generate();
        self.conn
            .execute(
                "INSERT INTO runs
                   (id, conversation_id, workspace_id, agent_id, model, status,
                    parent_run_id, resumed, invalidation_reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    id.as_str(),
                    new_run.conversation_id.as_str(),
                    new_run.workspace_id.as_str(),
                    new_run.agent_id.as_str(),
                    new_run.model,
                    new_run.parent_run_id.as_ref().map(|r| r.to_string()),
                    new_run.resumed as i64,
                    new_run.invalidation_reason.map(reason_to_str),
                    now_millis(),
                ],
            )
            .map_err(|e| ArgoError::Store(format!("insert run: {e}")))?;
        Ok(id)
    }

    /// Links a run to the assistant message it produces.
    pub fn attach_run_message(&self, run_id: &RunId, message_id: &MessageId) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET assistant_message_id = ?2 WHERE id = ?1",
                rusqlite::params![run_id.as_str(), message_id.as_str()],
            )
            .map_err(|e| ArgoError::Store(format!("attach run message: {e}")))?;
        Ok(())
    }

    /// Marks a run as running.
    pub fn mark_run_running(&self, run_id: &RunId) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET status = 'running' WHERE id = ?1",
                [run_id.as_str()],
            )
            .map_err(|e| ArgoError::Store(format!("mark running: {e}")))?;
        Ok(())
    }

    /// Records a terminal status.
    pub fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        error: Option<(&str, &str)>,
    ) -> Result<()> {
        if !status.is_terminal() {
            return Err(ArgoError::Invalid(format!(
                "{} is not a terminal status",
                status_to_str(status)
            )));
        }
        self.conn
            .execute(
                "UPDATE runs
                    SET status = ?2, error_code = ?3, error_message = ?4, finished_at = ?5
                  WHERE id = ?1",
                rusqlite::params![
                    run_id.as_str(),
                    status_to_str(status),
                    error.map(|(code, _)| code),
                    error.map(|(_, message)| message),
                    now_millis(),
                ],
            )
            .map_err(|e| ArgoError::Store(format!("finish run: {e}")))?;
        Ok(())
    }

    /// Loads a run.
    pub fn get_run(&self, run_id: &RunId) -> Result<Run> {
        self.conn
            .query_row(RUN_SELECT, [run_id.as_str()], map_run)
            .optional()
            .map_err(|e| ArgoError::Store(format!("get run: {e}")))?
            .transpose()?
            .ok_or_else(|| ArgoError::not_found("run", run_id.as_str()))
    }

    /// Lists runs that were still in flight, used for crash reconciliation.
    ///
    /// After an unclean shutdown these rows have no live process, so the daemon
    /// closes them out rather than leaving a conversation that looks busy forever.
    pub fn list_unfinished_runs(&self) -> Result<Vec<Run>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, conversation_id, workspace_id, agent_id, model, status,
                        assistant_message_id, parent_run_id, resumed, invalidation_reason,
                        error_code, error_message, created_at, finished_at
                   FROM runs WHERE status IN ('pending','running') ORDER BY created_at ASC",
            )
            .map_err(|e| ArgoError::Store(format!("prepare unfinished runs: {e}")))?;
        let rows = stmt
            .query_map([], map_run)
            .map_err(|e| ArgoError::Store(format!("list unfinished runs: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| ArgoError::Store(format!("read run: {e}")))??);
        }
        Ok(out)
    }

    /// Lists child runs spawned by a run.
    pub fn list_child_runs(&self, parent: &RunId) -> Result<Vec<Run>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, conversation_id, workspace_id, agent_id, model, status,
                        assistant_message_id, parent_run_id, resumed, invalidation_reason,
                        error_code, error_message, created_at, finished_at
                   FROM runs WHERE parent_run_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| ArgoError::Store(format!("prepare child runs: {e}")))?;
        let rows = stmt
            .query_map([parent.as_str()], map_run)
            .map_err(|e| ArgoError::Store(format!("list child runs: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| ArgoError::Store(format!("read child run: {e}")))??);
        }
        Ok(out)
    }

    /// Appends an event, assigning the next dense sequence number.
    pub fn append_event(&self, run_id: &RunId, kind: RunEventKind) -> Result<RunEvent> {
        let seq = self.next_event_seq(run_id)?;
        let event = RunEvent::new(run_id.clone(), seq, kind);
        let payload = serde_json::to_string(&event.kind)?;
        self.conn
            .execute(
                "INSERT INTO run_events (run_id, seq, at, payload) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![run_id.as_str(), seq, event.at, payload],
            )
            .map_err(|e| ArgoError::Store(format!("insert event: {e}")))?;
        Ok(event)
    }

    fn next_event_seq(&self, run_id: &RunId) -> Result<EventSeq> {
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(seq) FROM run_events WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| ArgoError::Store(format!("max event seq: {e}")))?
            .flatten();
        Ok(current.unwrap_or(0) + 1)
    }

    /// Lists events for a run after `after_seq`, in order.
    pub fn list_events_after(&self, run_id: &RunId, after_seq: EventSeq) -> Result<Vec<RunEvent>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, at, payload FROM run_events
                  WHERE run_id = ?1 AND seq > ?2 ORDER BY seq ASC",
            )
            .map_err(|e| ArgoError::Store(format!("prepare list events: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![run_id.as_str(), after_seq], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| ArgoError::Store(format!("list events: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (seq, at, payload) =
                row.map_err(|e| ArgoError::Store(format!("read event: {e}")))?;
            out.push(RunEvent {
                run_id: run_id.clone(),
                seq,
                at,
                kind: serde_json::from_str(&payload)?,
            });
        }
        Ok(out)
    }
}

const RUN_SELECT: &str = "SELECT id, conversation_id, workspace_id, agent_id, model, status,
        assistant_message_id, parent_run_id, resumed, invalidation_reason,
        error_code, error_message, created_at, finished_at
   FROM runs WHERE id = ?1";

fn map_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Run>> {
    let status_text: String = row.get(5)?;
    let run = Run {
        id: RunId::new(row.get::<_, String>(0)?),
        conversation_id: ConversationId::new(row.get::<_, String>(1)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(2)?),
        agent_id: AgentId::new(row.get::<_, String>(3)?),
        model: row.get(4)?,
        status: RunStatus::Pending,
        assistant_message_id: row.get::<_, Option<String>>(6)?.map(MessageId::new),
        parent_run_id: row.get::<_, Option<String>>(7)?.map(RunId::new),
        resumed: row.get::<_, i64>(8)? != 0,
        invalidation_reason: row.get(9)?,
        error_code: row.get(10)?,
        error_message: row.get(11)?,
        created_at: row.get(12)?,
        finished_at: row.get(13)?,
    };
    Ok(status_from_str(&status_text).map(|status| Run { status, ..run }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::event::TokenUsage;
    use argo_core::ids::SessionId;

    fn setup() -> (Store, ConversationId, WorkspaceId) {
        let store = Store::open_in_memory().expect("store");
        let ws = store.ensure_workspace(std::env::temp_dir()).expect("ws");
        let conv = store.create_conversation(&ws, None).expect("conv");
        (store, conv, ws)
    }

    fn new_run(conv: &ConversationId, ws: &WorkspaceId, agent: &str) -> NewRun {
        NewRun {
            conversation_id: conv.clone(),
            workspace_id: ws.clone(),
            agent_id: AgentId::new(agent),
            model: Some("m1".into()),
            resumed: false,
            invalidation_reason: None,
            parent_run_id: None,
        }
    }

    #[test]
    fn runs_start_pending_and_reach_a_terminal_status() {
        let (s, conv, ws) = setup();
        let id = s.create_run(new_run(&conv, &ws, "claude")).expect("create");
        assert_eq!(s.get_run(&id).expect("get").status, RunStatus::Pending);
        s.mark_run_running(&id).expect("running");
        assert_eq!(s.get_run(&id).expect("get").status, RunStatus::Running);
        s.finish_run(&id, RunStatus::Succeeded, None)
            .expect("finish");
        let run = s.get_run(&id).expect("get");
        assert_eq!(run.status, RunStatus::Succeeded);
        assert!(run.finished_at.is_some());
    }

    #[test]
    fn finishing_with_a_non_terminal_status_is_rejected() {
        let (s, conv, ws) = setup();
        let id = s.create_run(new_run(&conv, &ws, "claude")).expect("create");
        let err = s
            .finish_run(&id, RunStatus::Running, None)
            .expect_err("must reject");
        assert_eq!(err.code(), "INVALID_REQUEST");
    }

    #[test]
    fn failure_detail_is_recorded() {
        let (s, conv, ws) = setup();
        let id = s.create_run(new_run(&conv, &ws, "codex")).expect("create");
        s.finish_run(
            &id,
            RunStatus::Failed,
            Some(("AGENT_UNAVAILABLE", "codex not installed")),
        )
        .expect("finish");
        let run = s.get_run(&id).expect("get");
        assert_eq!(run.error_code.as_deref(), Some("AGENT_UNAVAILABLE"));
        assert_eq!(run.error_message.as_deref(), Some("codex not installed"));
    }

    #[test]
    fn run_records_why_a_session_was_reseeded() {
        // Surfacing the reason is what lets the TUI explain why a switch cost a
        // full context replay.
        let (s, conv, ws) = setup();
        let id = s
            .create_run(NewRun {
                invalidation_reason: Some(InvalidationReason::ModelChanged),
                ..new_run(&conv, &ws, "claude")
            })
            .expect("create");
        assert_eq!(
            s.get_run(&id).expect("get").invalidation_reason.as_deref(),
            Some("model_changed")
        );
    }

    #[test]
    fn cross_workspace_run_creation_is_rejected() {
        let (s, conv, _) = setup();
        let other_dir = std::env::temp_dir().join("argo-run-ws");
        std::fs::create_dir_all(&other_dir).expect("mkdir");
        let other_ws = s.ensure_workspace(&other_dir).expect("ws");
        let err = s
            .create_run(new_run(&conv, &other_ws, "claude"))
            .expect_err("must reject");
        assert_eq!(err.code(), "INVALID_REQUEST");
        std::fs::remove_dir_all(&other_dir).ok();
    }

    #[test]
    fn events_are_densely_sequenced_and_replayable_from_a_cursor() {
        let (s, conv, ws) = setup();
        let id = s.create_run(new_run(&conv, &ws, "claude")).expect("create");
        for i in 0..5 {
            s.append_event(
                &id,
                RunEventKind::TextDelta {
                    text: format!("chunk{i}"),
                },
            )
            .expect("append");
        }
        let all = s.list_events_after(&id, 0).expect("all");
        assert_eq!(all.len(), 5);
        assert_eq!(
            all.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );

        // A reconnecting client resumes from the last sequence it saw.
        let tail = s.list_events_after(&id, 3).expect("tail");
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].seq, 4);
    }

    #[test]
    fn all_event_kinds_survive_the_round_trip() {
        let (s, conv, ws) = setup();
        let id = s.create_run(new_run(&conv, &ws, "kiro")).expect("create");
        let kinds = vec![
            RunEventKind::RunStarted {
                agent_id: AgentId::new("kiro"),
                model: Some("auto".into()),
                resumed: true,
            },
            RunEventKind::ThinkingDelta { text: "t".into() },
            RunEventKind::ToolStarted {
                id: "t1".into(),
                name: "fs_read".into(),
                input: Some("{}".into()),
            },
            RunEventKind::ToolCompleted {
                id: "t1".into(),
                output: Some("ok".into()),
                ok: true,
            },
            RunEventKind::FileWritten {
                path: "a.rs".into(),
            },
            RunEventKind::PlanUpdated {
                steps: vec!["step".into()],
            },
            RunEventKind::SessionCaptured {
                session_id: SessionId::new("s1"),
            },
            RunEventKind::SessionReseeded {
                reason: "resume_failed".into(),
            },
            RunEventKind::Diagnostic {
                code: "D".into(),
                detail: "d".into(),
            },
            RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage: TokenUsage {
                    input: Some(5),
                    ..Default::default()
                },
            },
        ];
        for kind in &kinds {
            s.append_event(&id, kind.clone()).expect("append");
        }
        let stored = s.list_events_after(&id, 0).expect("list");
        assert_eq!(stored.len(), kinds.len());
        for (event, expected) in stored.iter().zip(kinds.iter()) {
            assert_eq!(&event.kind, expected);
        }
    }

    #[test]
    fn unfinished_runs_are_listed_for_crash_reconciliation() {
        let (s, conv, ws) = setup();
        let pending = s.create_run(new_run(&conv, &ws, "claude")).expect("a");
        let running = s.create_run(new_run(&conv, &ws, "codex")).expect("b");
        s.mark_run_running(&running).expect("running");
        let done = s.create_run(new_run(&conv, &ws, "grok")).expect("c");
        s.finish_run(&done, RunStatus::Succeeded, None)
            .expect("finish");

        let unfinished = s.list_unfinished_runs().expect("list");
        let ids: Vec<_> = unfinished.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains(&pending));
        assert!(ids.contains(&running));
        assert!(!ids.contains(&done));
    }

    #[test]
    fn child_runs_are_linked_to_their_parent() {
        let (s, conv, ws) = setup();
        let parent = s.create_run(new_run(&conv, &ws, "claude")).expect("parent");
        let child = s
            .create_run(NewRun {
                parent_run_id: Some(parent.clone()),
                ..new_run(&conv, &ws, "codex")
            })
            .expect("child");
        let children = s.list_child_runs(&parent).expect("list");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child);
        assert_eq!(children[0].agent_id, AgentId::new("codex"));
        assert!(s.list_child_runs(&child).expect("none").is_empty());
    }

    #[test]
    fn missing_run_is_not_found() {
        let (s, _, _) = setup();
        assert_eq!(
            s.get_run(&RunId::new("ghost"))
                .expect_err("must fail")
                .code(),
            "NOT_FOUND"
        );
    }
}
