//! End-to-end proof of Argo's cross-agent switching contract.
//!
//! These tests drive the store and the context engine exactly the way the daemon
//! will, without spawning any real CLI. They encode the behavior the product
//! promises:
//!
//! 1. Continuing with the same agent and model resumes the upstream session and
//!    sends only the new message.
//! 2. Switching CLI mid-conversation sends the remaining context to the new CLI.
//! 3. Switching model does the same, even on the same CLI.
//! 4. Switching back to an earlier CLI does not reuse its now-stale session.
//! 5. An agent with no native session (Grok) is reseeded every turn.

use argo_context::{compose_turn, ContextPackage, WorkspaceFacts, TRANSCRIPT_HEADING};
use argo_core::event::RunStatus;
use argo_core::ids::{AgentId, ConversationId, MessageId, SessionId, WorkspaceId};
use argo_core::message::ContentBlock;
use argo_core::session::{
    evaluate_resume, AgentSessionRecord, InvalidationReason, ResumeInputs, SelectionChange,
};
use argo_store::{NewMessage, NewRun, Store};

/// A test-only stand-in for a runtime adapter's declared capabilities.
struct Agent {
    id: &'static str,
    native_resume: bool,
}

const CLAUDE: Agent = Agent {
    id: "claude",
    native_resume: true,
};
const CODEX: Agent = Agent {
    id: "codex",
    native_resume: true,
};
const GROK: Agent = Agent {
    id: "grok",
    // Verified against xAI's CLI surface: no durable session to continue.
    native_resume: false,
};

struct Harness {
    store: Store,
    workspace: WorkspaceId,
    conversation: ConversationId,
    root: String,
}

impl Harness {
    fn new() -> Self {
        let store = Store::open_in_memory().expect("store");
        let workspace = store
            .ensure_workspace(std::env::temp_dir())
            .expect("workspace");
        let root = store.workspace_root(&workspace).expect("root");
        let conversation = store
            .create_conversation(&workspace, Some("switching"))
            .expect("conversation");
        Self {
            store,
            workspace,
            conversation,
            root,
        }
    }

    /// Records the user's pending selection, as a slash command would.
    fn select(&self, agent: &Agent, model: Option<&str>) {
        self.store
            .update_selection(
                &self.conversation,
                &SelectionChange {
                    agent_id: Some(AgentId::new(agent.id)),
                    model: model.map(|m| m.to_string()),
                    reasoning: None,
                },
            )
            .expect("select");
    }

    /// Runs one full turn and returns the body that would be sent to the CLI.
    ///
    /// Mirrors the daemon's ordering: append the user message, pin an assistant
    /// placeholder, evaluate the resume guard against the cursor, compose the
    /// body, then persist the reply and the upstream handle.
    fn turn(&self, agent: &Agent, model: Option<&str>, prompt: &str, reply: &str) -> String {
        let conv = &self.conversation;
        let agent_id = AgentId::new(agent.id);

        self.store
            .append_message(conv, NewMessage::user(prompt))
            .expect("user message");

        let stored = self
            .store
            .get_agent_session(conv, &agent_id)
            .expect("load session");

        let placeholder = MessageId::generate();
        let cursor = self
            .store
            .latest_completed_assistant_message_id(
                conv,
                Some(&placeholder),
                stored.as_ref().and_then(|s| s.last_message_id.as_ref()),
            )
            .expect("cursor");

        let plan = evaluate_resume(ResumeInputs {
            stored: stored.as_ref(),
            supports_resume: agent.native_resume,
            current_model: model,
            current_cwd: Some(&self.root),
            latest_completed_assistant: cursor.as_ref(),
        });

        // The remaining context: everything already in the conversation except the
        // turn being submitted.
        let history = self.store.list_messages(conv).expect("history");
        let package = ContextPackage {
            stable_instructions: "You are continuing work inside Argo.".into(),
            workspace: WorkspaceFacts {
                root: self.root.clone(),
                ..Default::default()
            },
            recent_messages: history
                .into_iter()
                .filter(|m| m.transferable_text() != prompt)
                .collect(),
            ..Default::default()
        };
        let body = compose_turn(&plan, &package, prompt);

        let run = self
            .store
            .create_run(NewRun {
                conversation_id: conv.clone(),
                workspace_id: self.workspace.clone(),
                agent_id: agent_id.clone(),
                model: model.map(|m| m.to_string()),
                resumed: plan.skip_transcript(),
                invalidation_reason: plan.invalidation,
                parent_run_id: None,
            })
            .expect("run");
        self.store.mark_run_running(&run).expect("running");

        let message = self
            .store
            .append_message(
                conv,
                NewMessage::assistant(
                    vec![ContentBlock::text(reply)],
                    agent_id.clone(),
                    model.map(|m| m.to_string()),
                    run.clone(),
                ),
            )
            .expect("assistant message");
        self.store
            .attach_run_message(&run, &message)
            .expect("attach");
        self.store
            .finish_run(&run, RunStatus::Succeeded, None)
            .expect("finish");

        // Only adapters with a durable handle store one; Grok never does.
        if agent.native_resume {
            let session_id = stored
                .as_ref()
                .filter(|_| plan.skip_transcript())
                .map(|s| s.session_id.clone())
                .unwrap_or_else(|| SessionId::new(format!("{}-session-1", agent.id)));
            self.store
                .upsert_agent_session(
                    conv,
                    &AgentSessionRecord {
                        agent_id,
                        session_id,
                        model: model.map(|m| m.to_string()),
                        cwd: Some(self.root.clone()),
                        stable_hash: Some("stable-v1".into()),
                        last_message_id: Some(message),
                        updated_at: 0,
                    },
                )
                .expect("persist session");
        }

        body
    }

    /// Resume decision that the next turn on `agent`/`model` would take.
    fn plan_for(&self, agent: &Agent, model: Option<&str>) -> argo_core::session::ResumePlan {
        let agent_id = AgentId::new(agent.id);
        let stored = self
            .store
            .get_agent_session(&self.conversation, &agent_id)
            .expect("load");
        let cursor = self
            .store
            .latest_completed_assistant_message_id(
                &self.conversation,
                None,
                stored.as_ref().and_then(|s| s.last_message_id.as_ref()),
            )
            .expect("cursor");
        evaluate_resume(ResumeInputs {
            stored: stored.as_ref(),
            supports_resume: agent.native_resume,
            current_model: model,
            current_cwd: Some(&self.root),
            latest_completed_assistant: cursor.as_ref(),
        })
    }
}

#[test]
fn first_turn_has_no_history_to_transfer() {
    let h = Harness::new();
    h.select(&CLAUDE, Some("sonnet"));
    let body = h.turn(
        &CLAUDE,
        Some("sonnet"),
        "add a health endpoint",
        "Added /health.",
    );
    // A first turn still carries the stable instructions and workspace facts, but
    // there is no prior conversation to replay.
    assert!(!body.contains(TRANSCRIPT_HEADING));
    assert!(body.contains("You are continuing work inside Argo."));
    assert!(body.trim_end().ends_with("add a health endpoint"));
}

#[test]
fn same_agent_and_model_resumes_and_sends_only_the_new_message() {
    let h = Harness::new();
    h.turn(
        &CLAUDE,
        Some("sonnet"),
        "add a health endpoint",
        "Added /health.",
    );

    let plan = h.plan_for(&CLAUDE, Some("sonnet"));
    assert!(plan.skip_transcript(), "expected native resume");

    let body = h.turn(&CLAUDE, Some("sonnet"), "now add tests", "Added tests.");
    assert_eq!(body, "now add tests");
    // The upstream session already holds this; re-sending would duplicate it.
    assert!(!body.contains("Added /health."));
}

#[test]
fn switching_cli_sends_the_remaining_context_on_the_next_message() {
    let h = Harness::new();
    h.turn(
        &CLAUDE,
        Some("sonnet"),
        "add a health endpoint",
        "Added /health.",
    );
    h.turn(&CLAUDE, Some("sonnet"), "now add tests", "Added tests.");

    // The user runs `/agent codex` and sends the next message.
    h.select(&CODEX, None);
    let plan = h.plan_for(&CODEX, Some("gpt-5.6"));
    assert!(!plan.skip_transcript());
    assert_eq!(plan.invalidation, None, "codex has no stored session yet");

    let body = h.turn(&CODEX, Some("gpt-5.6"), "now optimize it", "Optimized.");

    // Codex never participated in the earlier turns, so it receives them.
    assert!(body.contains(TRANSCRIPT_HEADING));
    assert!(body.contains("add a health endpoint"));
    assert!(body.contains("## assistant (claude)"));
    assert!(body.contains("Added /health."));
    assert!(body.contains("Added tests."));
    // The live request is last and clearly separated.
    assert!(body.contains("## Current request\nnow optimize it"));
}

#[test]
fn switching_model_on_the_same_cli_also_transfers_context() {
    let h = Harness::new();
    h.turn(&CODEX, Some("gpt-5.6"), "scaffold the crate", "Scaffolded.");

    // `/model` only, same agent.
    h.select(&CODEX, Some("gpt-5.6-codex"));
    let plan = h.plan_for(&CODEX, Some("gpt-5.6-codex"));
    assert_eq!(plan.invalidation, Some(InvalidationReason::ModelChanged));

    let body = h.turn(&CODEX, Some("gpt-5.6-codex"), "add docs", "Documented.");
    assert!(body.contains(TRANSCRIPT_HEADING));
    assert!(body.contains("Scaffolded."));
    assert!(body.contains("## Current request\nadd docs"));
}

#[test]
fn switching_back_does_not_reuse_a_stale_session() {
    let h = Harness::new();
    h.turn(
        &CLAUDE,
        Some("sonnet"),
        "add a health endpoint",
        "Added /health.",
    );
    h.turn(&CODEX, Some("gpt-5.6"), "optimize it", "Optimized.");

    // Claude's stored handle still exists, but Codex has completed a turn since,
    // so Claude's view of the conversation is behind.
    let plan = h.plan_for(&CLAUDE, Some("sonnet"));
    assert_eq!(
        plan.invalidation,
        Some(InvalidationReason::ConversationAdvanced)
    );
    assert!(
        plan.stored_session_id.is_some(),
        "handle retained for diagnostics"
    );

    let body = h.turn(&CLAUDE, Some("sonnet"), "review the change", "Reviewed.");
    assert!(body.contains(TRANSCRIPT_HEADING));
    assert!(body.contains("## assistant (codex)"));
    assert!(body.contains("Optimized."));
}

#[test]
fn an_agent_without_native_sessions_is_reseeded_every_turn() {
    let h = Harness::new();
    h.turn(
        &GROK,
        Some("grok-4.3"),
        "explain the repo",
        "It is a Rust workspace.",
    );

    let plan = h.plan_for(&GROK, Some("grok-4.3"));
    assert_eq!(plan.invalidation, Some(InvalidationReason::Unsupported));

    let body = h.turn(
        &GROK,
        Some("grok-4.3"),
        "now summarize the tests",
        "Summarized.",
    );
    assert!(body.contains(TRANSCRIPT_HEADING));
    assert!(body.contains("It is a Rust workspace."));
    // No handle is stored for an adapter that cannot resume.
    assert!(h
        .store
        .get_agent_session(&h.conversation, &AgentId::new("grok"))
        .expect("load")
        .is_none());
}

#[test]
fn three_way_switch_preserves_one_canonical_history() {
    let h = Harness::new();
    h.turn(
        &CLAUDE,
        Some("sonnet"),
        "start the parser",
        "Parser started.",
    );
    h.turn(
        &CODEX,
        Some("gpt-5.6"),
        "add error handling",
        "Errors handled.",
    );
    let body = h.turn(
        &GROK,
        Some("grok-4.3"),
        "explain what changed",
        "Explained.",
    );

    // Grok, arriving last, sees both prior agents' contributions attributed.
    assert!(body.contains("## assistant (claude)"));
    assert!(body.contains("## assistant (codex)"));
    assert!(body.contains("Parser started."));
    assert!(body.contains("Errors handled."));

    // One conversation holds every turn regardless of which CLI produced it.
    let all = h.store.list_messages(&h.conversation).expect("messages");
    assert_eq!(all.len(), 6, "3 user + 3 assistant");
    let agents: Vec<String> = all
        .iter()
        .filter_map(|m| m.agent_id.as_ref().map(|a| a.to_string()))
        .collect();
    assert_eq!(agents, vec!["claude", "codex", "grok"]);
}

#[test]
fn history_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("argo.sqlite");

    let workspace_root = dir.path().join("repo");
    std::fs::create_dir_all(&workspace_root).expect("mkdir");

    let conversation = {
        let store = Store::open(&db).expect("open");
        let ws = store.ensure_workspace(&workspace_root).expect("ws");
        let conv = store
            .create_conversation(&ws, Some("persisted"))
            .expect("conv");
        store
            .append_message(&conv, NewMessage::user("remember this"))
            .expect("append");
        conv
    };

    // Reopen exactly as a restarted daemon would.
    let store = Store::open(&db).expect("reopen");
    store.verify().expect("integrity");
    let messages = store.list_messages(&conversation).expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].transferable_text(), "remember this");
}

#[test]
fn a_failed_turn_by_another_agent_does_not_invalidate_a_good_session() {
    let h = Harness::new();
    h.turn(
        &CLAUDE,
        Some("sonnet"),
        "add a health endpoint",
        "Added /health.",
    );

    // Codex attempts a turn and fails before producing a completed reply.
    let run = h
        .store
        .create_run(NewRun {
            conversation_id: h.conversation.clone(),
            workspace_id: h.workspace.clone(),
            agent_id: AgentId::new("codex"),
            model: Some("gpt-5.6".into()),
            resumed: false,
            invalidation_reason: None,
            parent_run_id: None,
        })
        .expect("run");
    let msg = h
        .store
        .append_message(
            &h.conversation,
            NewMessage::assistant(vec![], AgentId::new("codex"), None, run.clone()),
        )
        .expect("placeholder");
    h.store.attach_run_message(&run, &msg).expect("attach");
    h.store
        .finish_run(&run, RunStatus::Failed, Some(("PROCESS_ERROR", "crashed")))
        .expect("fail");

    // Claude's session is still the newest completed turn, so it stays resumable
    // instead of paying a needless full-context replay.
    let plan = h.plan_for(&CLAUDE, Some("sonnet"));
    assert!(
        plan.skip_transcript(),
        "a failed turn must not advance the resume cursor"
    );
}
