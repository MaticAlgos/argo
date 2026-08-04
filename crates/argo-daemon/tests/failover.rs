//! End-to-end backup-agent failover.
//!
//! The unit tests cover the decision in isolation; this drives the real thing —
//! `run_turn` against stub executables standing in for two coding CLIs, one of
//! which reports an exhausted plan. That is the only way to prove the parts that
//! matter in practice: that the handoff reuses one run, that the user's message
//! is not recorded twice, and that the reply is attributed to whoever answered.

use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{AgentId, ConversationId, SessionId};
use argo_core::session::{AgentSessionRecord, InvalidationReason};
use argo_core::ArgoPaths;
use argo_daemon::engine::{run_turn, FailoverPlan, TurnRequest};
use argo_runtime::exec::CancelToken;
use argo_store::Store;
use std::sync::{Arc, Mutex};

/// Writes an executable stub that ignores its arguments and prints `lines`.
///
/// Argo takes the executable from `TurnRequest::bin`, so a stub here exercises
/// the genuine adapter argv, stream parser, and engine paths.
fn stub(dir: &std::path::Path, name: &str, lines: &[&str]) -> String {
    let path = dir.join(name);
    let body = lines
        .iter()
        .map(|line| format!("cat <<'ARGO_EOF'\n{line}\nARGO_EOF"))
        .collect::<Vec<_>>()
        .join("\n");
    // Draining stdin keeps the stub from dying on SIGPIPE when Argo writes the
    // prompt frame to a process that has already exited.
    std::fs::write(
        &path,
        format!("#!/bin/sh\ncat >/dev/null &\n{body}\nexit 0\n"),
    )
    .expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path.to_string_lossy().to_string()
}

/// A Claude-format terminal frame reporting an exhausted plan.
const EXHAUSTED: &str = r#"{"type":"result","is_error":true,"num_turns":0,"duration_api_ms":0,"result":"Claude usage limit reached. Your limit will reset at 3pm."}"#;

/// A Codex-format successful reply.
///
/// The standby speaks its own adapter's stream format, not the primary's —
/// which is exactly what a real handoff has to cope with.
const ANSWERED: &[&str] = &[
    r#"{"type":"item.completed","item":{"type":"agent_message","id":"m1","text":"picked up by the standby"}}"#,
    r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":4}}"#,
];

const BACKUP_FAILED: &str =
    r#"{"type":"turn.failed","error":{"message":"backup model unavailable"}}"#;

struct Harness {
    store: Arc<Mutex<Store>>,
    paths: ArgoPaths,
    conversation: ConversationId,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("argo.sqlite")).expect("store");
    let paths = ArgoPaths::with_root(dir.path().join("data"));
    paths.ensure_dirs().expect("dirs");
    let workspace = store.ensure_workspace(dir.path()).expect("workspace");
    let conversation = store.create_conversation(&workspace, None).expect("conv");
    Harness {
        store: Arc::new(Mutex::new(store)),
        paths,
        conversation,
        _dir: dir,
    }
}

fn request(conversation: &ConversationId, agent: &str, bin: String, prompt: &str) -> TurnRequest {
    TurnRequest {
        conversation_id: conversation.clone(),
        parent_run_id: None,
        delegation_allowed: false,
        prompt: prompt.into(),
        agent_id: AgentId::new(agent),
        model: None,
        reasoning: None,
        bin,
        help_flags: vec![],
        active_skills: vec![],
        active_mcp_servers: vec![],
        project_instructions: None,
        mcp_descriptors: vec![],
        mcp_config: None,
        mcp_overrides: vec![],
        mcp_environment: vec![],
        timeout_ms: Some(20_000),
        mode: argo_core::mode::AgentMode::Full,
        append_user: true,
        failover: None,
    }
}

fn plan(agent: &str, bin: String) -> FailoverPlan {
    FailoverPlan {
        agent_id: AgentId::new(agent),
        model: None,
        reasoning: None,
        bin,
        help_flags: vec![],
        active_skills: vec![],
        active_mcp_servers: vec![],
        mcp_descriptors: vec![],
        mcp_config: None,
        mcp_overrides: vec![],
        mcp_environment: vec![],
    }
}

#[tokio::test]
async fn an_exhausted_plan_hands_the_turn_to_the_backup_within_one_run() {
    let h = harness();
    let dir = h.paths.root();
    let spent = stub(dir, "spent-cli", &[EXHAUSTED]);
    let standby = stub(dir, "standby-cli", ANSWERED);

    // A stale standby session forces a fresh backup attempt for a reason that is
    // different from the primary's plan. The rebound run must retain this
    // provenance rather than the primary's original resume fields.
    h.store
        .lock()
        .expect("lock")
        .upsert_agent_session(
            &h.conversation,
            &AgentSessionRecord {
                agent_id: AgentId::new("codex"),
                session_id: SessionId::new("old-codex-session"),
                model: Some("old-model".into()),
                cwd: None,
                stable_hash: None,
                last_message_id: None,
                updated_at: 0,
            },
        )
        .expect("standby session");

    let mut turn = request(&h.conversation, "claude", spent, "do the thing");
    turn.failover = Some(plan("codex", standby));

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let listener =
        move |event: argo_core::event::RunEvent| sink.lock().expect("lock").push(event.kind);

    let outcome = run_turn(
        &h.store,
        &h.paths,
        turn,
        &CancelToken::new(),
        Some(&listener),
    )
    .await
    .expect("turn runs");

    assert_eq!(outcome.status, RunStatus::Succeeded, "standby must answer");
    assert!(!outcome.resumed);
    assert_eq!(outcome.invalidation, Some(InvalidationReason::ModelChanged));

    let events = events.lock().expect("lock");
    let failover = events.iter().find_map(|kind| match kind {
        RunEventKind::BackupFailover { detail, .. } => Some(detail),
        _ => None,
    });
    let detail = failover.expect("a failover diagnostic must be emitted");
    assert!(detail.contains("claude"), "{detail}");
    assert!(detail.contains("codex"), "{detail}");

    // One run and exactly one terminal event: clients follow a single run id and
    // stop at its first terminal event, so a second run would be invisible.
    let terminal = events
        .iter()
        .filter(|kind| matches!(kind, RunEventKind::RunFinished { .. }))
        .count();
    assert_eq!(
        terminal, 1,
        "the handoff must not split the turn into two runs"
    );

    let store = h.store.lock().expect("lock");
    let messages = store.list_messages(&h.conversation).expect("messages");
    let users: Vec<_> = messages
        .iter()
        .filter(|m| m.role == argo_core::message::Role::User)
        .collect();
    assert_eq!(users.len(), 1, "the prompt must be recorded exactly once");

    let assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == argo_core::message::Role::Assistant)
        .expect("an assistant message");
    // Credit belongs to whoever actually answered, not to the CLI that ran dry.
    assert_eq!(
        assistant.agent_id.as_ref().map(|id| id.to_string()),
        Some("codex".to_string())
    );
    assert!(
        assistant.transferable_text().contains("standby"),
        "{:?}",
        assistant.transferable_text()
    );

    let run = store.get_run(&outcome.run_id).expect("run");
    assert_eq!(run.agent_id.to_string(), "codex", "run must be rebound");
    assert!(!run.resumed);
    assert_eq!(run.invalidation_reason.as_deref(), Some("model_changed"));

    // The spent agent becomes the standby: if the new primary also runs dry the
    // original is retried, by which time its limit may have reset.
    let conversation = store
        .get_conversation(&h.conversation)
        .expect("conversation");
    assert_eq!(conversation.selected_agent_id.as_deref(), Some("codex"));
    assert_eq!(
        conversation.selected_backup_agent_id.as_deref(),
        Some("claude")
    );
}

#[tokio::test]
async fn a_failed_backup_is_not_promoted() {
    let h = harness();
    let spent = stub(h.paths.root(), "spent-cli", &[EXHAUSTED]);
    let failed_backup = stub(h.paths.root(), "failed-backup-cli", &[BACKUP_FAILED]);
    {
        let store = h.store.lock().expect("lock");
        store
            .update_selection(
                &h.conversation,
                &argo_core::session::SelectionChange {
                    agent_id: Some(AgentId::new("claude")),
                    ..Default::default()
                },
            )
            .expect("primary");
        store
            .set_backup_agent(&h.conversation, Some("codex"), None, None)
            .expect("backup");
    }

    let mut turn = request(&h.conversation, "claude", spent, "do the thing");
    turn.failover = Some(plan("codex", failed_backup));
    let outcome = run_turn(&h.store, &h.paths, turn, &CancelToken::new(), None)
        .await
        .expect("turn completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    let conversation = h
        .store
        .lock()
        .expect("lock")
        .get_conversation(&h.conversation)
        .expect("conversation");
    assert_eq!(conversation.selected_agent_id.as_deref(), Some("claude"));
    assert_eq!(
        conversation.selected_backup_agent_id.as_deref(),
        Some("codex")
    );
}

#[tokio::test]
async fn a_newer_user_selection_wins_over_successful_failover_promotion() {
    let h = harness();
    let spent = stub(h.paths.root(), "spent-cli", &[EXHAUSTED]);
    let standby = stub(h.paths.root(), "standby-cli", ANSWERED);
    {
        let store = h.store.lock().expect("lock");
        store
            .update_selection(
                &h.conversation,
                &argo_core::session::SelectionChange {
                    agent_id: Some(AgentId::new("claude")),
                    ..Default::default()
                },
            )
            .expect("primary");
        store
            .set_backup_agent(&h.conversation, Some("codex"), None, None)
            .expect("backup");
    }

    let mut turn = request(&h.conversation, "claude", spent, "do the thing");
    turn.failover = Some(plan("codex", standby));
    let listener_store = Arc::clone(&h.store);
    let listener_conversation = h.conversation.clone();
    let listener = move |event: argo_core::event::RunEvent| {
        if matches!(event.kind, RunEventKind::BackupFailover { .. }) {
            listener_store
                .lock()
                .expect("lock")
                .update_selection(
                    &listener_conversation,
                    &argo_core::session::SelectionChange {
                        agent_id: Some(AgentId::new("kiro")),
                        model: Some("auto".into()),
                        reasoning: None,
                    },
                )
                .expect("newer selection");
        }
    };

    let outcome = run_turn(
        &h.store,
        &h.paths,
        turn,
        &CancelToken::new(),
        Some(&listener),
    )
    .await
    .expect("turn completes");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    let conversation = h
        .store
        .lock()
        .expect("lock")
        .get_conversation(&h.conversation)
        .expect("conversation");
    assert_eq!(conversation.selected_agent_id.as_deref(), Some("kiro"));
    assert_eq!(conversation.selected_model.as_deref(), Some("auto"));
    assert_eq!(
        conversation.selected_backup_agent_id.as_deref(),
        Some("codex")
    );
}

#[tokio::test]
async fn without_a_backup_an_exhausted_plan_still_just_fails() {
    // The default path for every conversation that never configured failover.
    let h = harness();
    let spent = stub(h.paths.root(), "spent-cli", &[EXHAUSTED]);
    let turn = request(&h.conversation, "claude", spent, "do the thing");

    let outcome = run_turn(&h.store, &h.paths, turn, &CancelToken::new(), None)
        .await
        .expect("turn completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    let store = h.store.lock().expect("lock");
    let conversation = store
        .get_conversation(&h.conversation)
        .expect("conversation");
    assert!(conversation.selected_backup_agent_id.is_none());
}

#[tokio::test]
async fn an_ordinary_failure_does_not_spend_the_backups_quota() {
    // Only exhaustion justifies handing the turn to another vendor; a plain
    // model error is the user's to act on.
    let h = harness();
    let dir = h.paths.root();
    let failing = stub(
        dir,
        "failing-cli",
        &[
            r#"{"type":"result","is_error":true,"num_turns":0,"duration_api_ms":0,"result":"invalid model name"}"#,
        ],
    );
    let standby = stub(dir, "standby-cli", ANSWERED);

    let mut turn = request(&h.conversation, "claude", failing, "do the thing");
    turn.failover = Some(plan("codex", standby));

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let listener =
        move |event: argo_core::event::RunEvent| sink.lock().expect("lock").push(event.kind);

    let outcome = run_turn(
        &h.store,
        &h.paths,
        turn,
        &CancelToken::new(),
        Some(&listener),
    )
    .await
    .expect("turn completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(
        !events
            .lock()
            .expect("lock")
            .iter()
            .any(|kind| matches!(kind, RunEventKind::BackupFailover { .. })),
        "a non-exhaustion failure must not trigger failover"
    );

    let store = h.store.lock().expect("lock");
    let conversation = store
        .get_conversation(&h.conversation)
        .expect("conversation");
    assert!(
        conversation.selected_agent_id.is_none(),
        "selection must be untouched"
    );
}
