//! Slash commands.
//!
//! Every configuration action in Argo is a `/` command, so the running
//! conversation is where you change agents, models, and resources. Parsing is
//! pure and validated against the runtime registry, which means an invalid model
//! is rejected with the valid set rather than failing later at spawn time.

use argo_core::ids::AgentId;
use argo_core::session::SelectionChange;

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Show the command reference.
    Help,
    /// Open the agent picker, or switch directly.
    Agent(Option<String>),
    /// Open the model picker, or switch directly.
    Model(Option<String>),
    /// Set reasoning effort.
    Effort(Option<String>),
    /// Set or cycle the execution mode.
    Mode(Option<String>),
    /// Show the detected agent inventory.
    Agents,
    /// List discovered skills.
    Skills,
    /// List configured MCP servers.
    Mcp,
    /// Show what the next turn would send.
    Context,
    /// Resume a session: list them, or open one directly.
    Resume(Option<String>),
    /// Start a new conversation.
    New(Option<String>),

    /// Show child conversations from delegation.
    Children,
    /// Delegate a task to another agent.
    Delegate {
        /// Target agent id.
        agent: String,
        /// Task description.
        task: String,
    },
    /// Cancel the active run.
    Cancel,
    /// Show settings and paths.
    Config,
    /// Run diagnostics.
    Doctor,
    /// Leave the TUI.
    Quit,
}

/// Why a line could not be parsed as a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The command name is not recognized.
    Unknown(String),
    /// A required argument was missing.
    MissingArgument {
        /// Command name.
        command: &'static str,
        /// What was expected.
        expected: &'static str,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(name) => write!(
                f,
                "unknown command /{name}. Type /help to see what is available"
            ),
            Self::MissingArgument { command, expected } => {
                write!(f, "/{command} needs {expected}")
            }
        }
    }
}

/// True when `line` should be treated as a command rather than a message.
///
/// A lone `/` is treated as text so a user typing a path fragment is not told
/// their message is an unknown command.
pub fn is_command(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('/') && trimmed.len() > 1
}

/// Parses a command line.
pub fn parse(line: &str) -> std::result::Result<Command, ParseError> {
    let trimmed = line.trim().trim_start_matches('/');
    let (name, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (trimmed, ""),
    };
    let argument = (!rest.is_empty()).then(|| rest.to_string());

    match name.to_ascii_lowercase().as_str() {
        "help" | "?" => Ok(Command::Help),
        "agent" => Ok(Command::Agent(argument)),
        "model" => Ok(Command::Model(argument)),
        "effort" | "reasoning" => Ok(Command::Effort(argument)),
        "mode" | "plan" => Ok(Command::Mode(argument)),
        "agents" => Ok(Command::Agents),
        "skills" => Ok(Command::Skills),
        "mcp" => Ok(Command::Mcp),
        "context" => Ok(Command::Context),
        // One command for both listing and opening: two commands for one intent
        // was needless surface.
        "resume" | "chats" | "sessions" | "conversations" | "open" => Ok(Command::Resume(argument)),
        "new" => Ok(Command::New(argument)),

        "children" => Ok(Command::Children),
        "delegate" => {
            let rest = argument.ok_or(ParseError::MissingArgument {
                command: "delegate",
                expected: "an agent and a task, for example /delegate codex review my changes",
            })?;
            let (agent, task) =
                rest.split_once(char::is_whitespace)
                    .ok_or(ParseError::MissingArgument {
                        command: "delegate",
                        expected: "a task after the agent name",
                    })?;
            Ok(Command::Delegate {
                agent: agent.to_string(),
                task: task.trim().to_string(),
            })
        }
        "cancel" | "stop" => Ok(Command::Cancel),
        "config" | "settings" => Ok(Command::Config),
        "doctor" => Ok(Command::Doctor),
        "quit" | "exit" | "q" => Ok(Command::Quit),
        other => Err(ParseError::Unknown(other.to_string())),
    }
}

/// Command names offered for completion, in display order.
pub const COMMAND_NAMES: &[&str] = &[
    "/help",
    "/agent",
    "/model",
    "/effort",
    "/mode",
    "/agents",
    "/skills",
    "/mcp",
    "/context",
    "/resume",
    "/new",
    "/children",
    "/delegate",
    "/cancel",
    "/config",
    "/doctor",
    "/quit",
];

/// Completion candidates for a partially typed command.
pub fn complete(prefix: &str) -> Vec<&'static str> {
    // A multi-line message is prose, not a command, even if it opens with a
    // slash. Trimming first would hide the newline and offer commands anyway.
    if prefix.contains('\n') {
        return Vec::new();
    }
    let prefix = prefix.trim();
    if !prefix.starts_with('/') {
        return Vec::new();
    }
    // Once a full name plus a space is typed, the user is entering arguments and
    // further name completion would be noise.
    if prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut matches: Vec<&'static str> = COMMAND_NAMES
        .iter()
        .filter(|name| name.starts_with(prefix))
        .copied()
        .collect();
    // Sorted so the list is stable and predictable as the prefix narrows.
    matches.sort_unstable();
    matches
}

/// One line of the command reference.
pub struct HelpEntry {
    /// Usage form.
    pub usage: &'static str,
    /// What it does.
    pub detail: &'static str,
}

/// The command reference shown by `/help`.
pub fn help() -> Vec<HelpEntry> {
    vec![
        HelpEntry {
            usage: "/help",
            detail: "show this reference",
        },
        HelpEntry {
            usage: "/agent [id]",
            detail: "switch coding CLI; applies to your next message",
        },
        HelpEntry {
            usage: "/model [id]",
            detail: "switch model; applies to your next message",
        },
        HelpEntry {
            usage: "/effort [level]",
            detail: "set reasoning effort where the model supports it",
        },
        HelpEntry {
            usage: "/mode [id]",
            detail: "switch execution mode (Shift+Tab cycles): full, plan, accept-edits",
        },
        HelpEntry {
            usage: "/agents",
            detail: "show detected CLIs, versions, and limitations",
        },
        HelpEntry {
            usage: "/skills",
            detail: "list skills available to every agent",
        },
        HelpEntry {
            usage: "/mcp",
            detail: "list configured MCP servers",
        },
        HelpEntry {
            usage: "/context",
            detail: "show exactly what your next message will send",
        },
        HelpEntry {
            usage: "/resume [n|id]",
            detail: "list earlier sessions, or reopen one",
        },
        HelpEntry {
            usage: "/new [title]",
            detail: "start a new conversation",
        },
        HelpEntry {
            usage: "/children",
            detail: "show conversations spawned by delegation",
        },
        HelpEntry {
            usage: "/delegate <agent> <task>",
            detail: "hand a task to a different CLI as a subagent",
        },
        HelpEntry {
            usage: "/cancel",
            detail: "stop the running turn",
        },
        HelpEntry {
            usage: "/config",
            detail: "show settings and file locations",
        },
        HelpEntry {
            usage: "/doctor",
            detail: "run diagnostics",
        },
        HelpEntry {
            usage: "/quit",
            detail: "leave Argo (the daemon keeps running)",
        },
    ]
}

/// Validates an agent id against the registry.
///
/// Returns the canonical id, or a message naming the valid options.
pub fn resolve_agent(input: &str) -> std::result::Result<AgentId, String> {
    let wanted = input.trim().to_ascii_lowercase();
    match argo_runtime::find(&wanted) {
        Some(def) => Ok(AgentId::new(def.id)),
        None => Err(format!(
            "unknown agent '{input}'. Available: {}",
            argo_runtime::ids().join(", ")
        )),
    }
}

/// Builds the selection change for an agent switch.
pub fn agent_change(agent: AgentId) -> SelectionChange {
    SelectionChange {
        agent_id: Some(agent),
        model: None,
        reasoning: None,
    }
}

/// Builds the selection change for a model switch.
pub fn model_change(model: impl Into<String>) -> SelectionChange {
    SelectionChange {
        agent_id: None,
        model: Some(model.into()),
        reasoning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_commands_but_not_bare_slashes() {
        assert!(is_command("/agent"));
        assert!(is_command("  /model opus"));
        // A lone slash is text, not a mistyped command.
        assert!(!is_command("/"));
        assert!(!is_command("just a message"));
        assert!(!is_command("path/to/file"));
    }

    #[test]
    fn parses_bare_and_direct_forms() {
        assert_eq!(parse("/agent").expect("parse"), Command::Agent(None));
        assert_eq!(
            parse("/agent codex").expect("parse"),
            Command::Agent(Some("codex".into()))
        );
        assert_eq!(parse("/model").expect("parse"), Command::Model(None));
        assert_eq!(
            parse("/model gpt-5.6-codex").expect("parse"),
            Command::Model(Some("gpt-5.6-codex".into()))
        );
    }

    #[test]
    fn accepts_aliases_and_is_case_insensitive() {
        assert_eq!(parse("/AGENTS").expect("parse"), Command::Agents);
        assert_eq!(parse("/q").expect("parse"), Command::Quit);
        assert_eq!(parse("/exit").expect("parse"), Command::Quit);
        assert_eq!(parse("/stop").expect("parse"), Command::Cancel);
        assert_eq!(parse("/?").expect("parse"), Command::Help);
        assert_eq!(
            parse("/reasoning high").expect("parse"),
            Command::Effort(Some("high".into()))
        );
        assert_eq!(
            parse("/conversations").expect("parse"),
            Command::Resume(None)
        );
    }

    #[test]
    fn parses_delegate_into_agent_and_task() {
        assert_eq!(
            parse("/delegate codex review my changes").expect("parse"),
            Command::Delegate {
                agent: "codex".into(),
                task: "review my changes".into()
            }
        );
    }

    #[test]
    fn delegate_without_a_task_is_rejected_with_guidance() {
        let error = parse("/delegate").expect_err("must fail");
        assert!(error.to_string().contains("/delegate codex"));
        let error = parse("/delegate codex").expect_err("must fail");
        assert!(error.to_string().contains("task after the agent"));
    }

    #[test]
    fn resume_lists_without_an_argument_and_opens_with_one() {
        assert_eq!(parse("/resume").expect("parse"), Command::Resume(None));
        assert_eq!(
            parse("/resume 2").expect("parse"),
            Command::Resume(Some("2".into()))
        );
        // The old spellings still work so muscle memory is not punished.
        for alias in ["/chats", "/sessions", "/conversations", "/open"] {
            assert!(matches!(
                parse(alias).expect("parse"),
                Command::Resume(None)
            ));
        }
    }

    #[test]
    fn unknown_commands_point_at_help() {
        let error = parse("/launch").expect_err("must fail");
        assert_eq!(error, ParseError::Unknown("launch".into()));
        assert!(error.to_string().contains("/help"));
    }

    #[test]
    fn completion_narrows_as_you_type() {
        assert!(complete("/a").contains(&"/agent"));
        assert!(complete("/a").contains(&"/agents"));
        assert_eq!(complete("/dele"), vec!["/delegate"]);
        assert!(complete("/zzz").is_empty());
        // Not a command, or already past the name.
        assert!(complete("hello").is_empty());
        assert!(complete("/agent co").is_empty());
        // A multi-line message is prose even when it starts with a slash.
        assert!(complete("/agent\nmore text").is_empty());
    }

    #[test]
    fn agent_ids_are_validated_against_the_registry() {
        assert_eq!(
            resolve_agent("codex").expect("resolve").to_string(),
            "codex"
        );
        assert_eq!(
            resolve_agent(" CLAUDE ").expect("resolve").to_string(),
            "claude"
        );
        let error = resolve_agent("gemini").expect_err("must fail");
        // The error must tell the user what they can pick instead.
        assert!(error.contains("claude"));
        assert!(error.contains("grok"));
    }

    #[test]
    fn selection_changes_isolate_what_they_change() {
        let agent = agent_change(AgentId::new("codex"));
        assert_eq!(agent.agent_id, Some(AgentId::new("codex")));
        // Switching agent must not assert a model; the store clears the stale one.
        assert!(agent.model.is_none());

        let model = model_change("opus");
        assert!(model.agent_id.is_none());
        assert_eq!(model.model.as_deref(), Some("opus"));
    }

    #[test]
    fn every_listed_command_name_parses() {
        // Guards against a name appearing in help or completion that the parser
        // does not accept.
        for name in COMMAND_NAMES {
            let line = match *name {
                "/delegate" => "/delegate codex do it".to_string(),
                other => other.to_string(),
            };
            assert!(parse(&line).is_ok(), "{name} must parse");
        }
    }

    #[test]
    fn help_covers_every_command_name() {
        let entries = help();
        for name in COMMAND_NAMES {
            let bare = name.trim_start_matches('/');
            assert!(
                entries.iter().any(|e| e.usage.contains(bare)),
                "{name} is missing from /help"
            );
        }
    }
}
