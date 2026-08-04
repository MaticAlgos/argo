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
    /// Configure or clear the CLI/model used for new conversations.
    Default(DefaultCommand),
    /// Set or cycle the execution mode.
    Mode(Option<String>),
    /// Choose the standby CLI this conversation fails over to, or clear it.
    Backup(Option<String>),
    /// Set up or inspect the Telegram bridge.
    Telegram(Option<String>),
    /// Show exact token usage reported for the last completed turn.
    Usage,
    /// Show current Argo conversation/run state.
    Status,
    /// Check for or install a newer Argo build.
    Update(UpdateCommand),
    /// Show the detected agent inventory.
    Agents,
    /// List discovered skills.
    Skills,
    /// Manage project-local automatic instructions.
    Instructions(InstructionsCommand),
    /// Show or change whether agent thinking and tool activity are rendered.
    ///
    /// One toggle covers both: they are the same class of intermediate detail,
    /// and hiding reasoning while leaving tool calls on screen hides nothing.
    Thinking(ThinkingCommand),
    /// Inspect or manage configured MCP servers.
    Mcp(McpCommand),
    /// Show what the next turn would send.
    Context,
    /// Fold the conversation so far into a summary to free context.
    Compact,
    /// Resume a session: list them, or open one directly.
    Resume(Option<String>),
    /// Start a new conversation.
    New(Option<String>),
    /// Delete stored conversations for the current workspace.
    ClearHistory,

    /// Show child conversations from delegation.
    Children,
    /// Return from a directly opened child conversation to its parent.
    Parent,
    /// Delegate a task to another agent.
    Delegate {
        /// Target agent id.
        agent: String,
        /// Task description.
        task: String,
    },
    /// Review the messages waiting to be sent.
    Queue,
    /// Cancel the active run.
    Cancel,
    /// Show settings and paths.
    Config,
    /// Run diagnostics.
    Doctor,
    /// Leave the TUI.
    Quit,
}

/// A visibility change requested through `/thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingCommand {
    /// Render thinking and tool-activity lines in the transcript.
    Show,
    /// Hide thinking and tool-activity lines from the transcript.
    Hide,
    /// Invert the current visibility setting.
    Toggle,
}

/// An operation requested through `/instructions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionsCommand {
    /// Open the enable/disable/edit picker.
    Menu,
    /// Capture and inject durable project instructions.
    Enable,
    /// Stop capture and injection while retaining the file.
    Disable,
    /// Open the project instruction file in the user's editor.
    Edit,
}

/// An operation requested through `/default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultCommand {
    /// Open the CLI → model → effort configuration flow.
    Configure,
    /// Save the current complete selection.
    Current,
    /// Remove the startup selection.
    Clear,
}

/// An operation requested through `/update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCommand {
    /// Compare this build with the version published on GitHub.
    Check,
    /// Exit the TUI and install a newer published build when available.
    Install,
    /// Exit and reinstall the published build even at the same version.
    Force,
}

/// An operation requested through `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    /// List configured MCP servers and their connection state.
    List,
    /// Open the guided server setup flow.
    Add,
    /// Check every server, or one named server when supplied.
    Check(Option<String>),
    /// Reconnect a named server.
    Reconnect(String),
    /// Authenticate a named server.
    Login(String),
    /// Clear authentication for a named server.
    Logout(String),
    /// Delete a named server from the configuration.
    Remove(String),
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
    /// An argument was present, but is not one of the supported values.
    InvalidArgument {
        /// Command name.
        command: &'static str,
        /// The unsupported argument.
        value: String,
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
            Self::InvalidArgument {
                command,
                value,
                expected,
            } => write!(
                f,
                "invalid /{command} argument '{value}'; expected {expected}"
            ),
        }
    }
}

/// The command name a line opens with, when it opens with one at all.
///
/// `None` for a lone `/`, and for anything carrying a separator inside the name:
/// dragging a file into the composer pastes an absolute path, and
/// `/var/folders/…/Screenshot.png` is a message about a file rather than a
/// mistyped command. Command names never contain a slash, so the second
/// separator is what tells the two apart.
fn command_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('/')?;
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// True when `line` should be treated as a command rather than a message.
pub fn is_command(line: &str) -> bool {
    command_name(line).is_some()
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
        "default" => parse_default(rest),
        "mode" | "plan" => Ok(Command::Mode(argument)),
        "backup" | "fallback" => Ok(Command::Backup(argument)),
        "telegram" | "tg" => Ok(Command::Telegram(argument)),
        "usage" => Ok(Command::Usage),
        "status" => Ok(Command::Status),
        "update" | "upgrade" => parse_update(rest),
        "agents" => Ok(Command::Agents),
        "skills" => Ok(Command::Skills),
        "instructions" => parse_instructions(rest),
        "thinking" => parse_thinking(rest),
        "mcp" => parse_mcp(rest),
        "context" => Ok(Command::Context),
        "compact" => Ok(Command::Compact),
        // One command for both listing and opening: two commands for one intent
        // was needless surface.
        "resume" | "chats" | "sessions" | "conversations" | "open" => Ok(Command::Resume(argument)),
        "new" => Ok(Command::New(argument)),
        "clear-history" | "clear-chats" => Ok(Command::ClearHistory),

        "children" => Ok(Command::Children),
        "parent" | "back" => Ok(Command::Parent),
        "delegate" => {
            let rest = argument.ok_or(ParseError::MissingArgument {
                command: "delegate",
                expected: "an explicit target and task: /delegate <agent> <task>",
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
        "queue" | "queued" | "pending" => Ok(Command::Queue),
        "cancel" | "stop" => Ok(Command::Cancel),
        "config" | "settings" => Ok(Command::Config),
        "doctor" => Ok(Command::Doctor),
        "quit" | "exit" | "q" => Ok(Command::Quit),
        other => Err(ParseError::Unknown(other.to_string())),
    }
}

fn parse_default(rest: &str) -> std::result::Result<Command, ParseError> {
    match rest.to_ascii_lowercase().as_str() {
        "" | "configure" | "set" => Ok(Command::Default(DefaultCommand::Configure)),
        "current" => Ok(Command::Default(DefaultCommand::Current)),
        "clear" | "remove" => Ok(Command::Default(DefaultCommand::Clear)),
        _ => Err(ParseError::InvalidArgument {
            command: "default",
            value: rest.to_string(),
            expected: "configure, current, or clear",
        }),
    }
}

fn parse_update(rest: &str) -> std::result::Result<Command, ParseError> {
    match rest.to_ascii_lowercase().as_str() {
        "" | "check" => Ok(Command::Update(UpdateCommand::Check)),
        "install" | "now" => Ok(Command::Update(UpdateCommand::Install)),
        "force" | "reinstall" => Ok(Command::Update(UpdateCommand::Force)),
        _ => Err(ParseError::InvalidArgument {
            command: "update",
            value: rest.to_string(),
            expected: "check, install, or force",
        }),
    }
}

fn parse_thinking(rest: &str) -> std::result::Result<Command, ParseError> {
    match rest.to_ascii_lowercase().as_str() {
        "" | "toggle" => Ok(Command::Thinking(ThinkingCommand::Toggle)),
        "show" => Ok(Command::Thinking(ThinkingCommand::Show)),
        "hide" => Ok(Command::Thinking(ThinkingCommand::Hide)),
        _ => Err(ParseError::InvalidArgument {
            command: "thinking",
            value: rest.to_string(),
            expected: "show, hide, or toggle",
        }),
    }
}

fn parse_instructions(rest: &str) -> std::result::Result<Command, ParseError> {
    let action = match rest.to_ascii_lowercase().as_str() {
        "" => InstructionsCommand::Menu,
        "enable" => InstructionsCommand::Enable,
        "disable" => InstructionsCommand::Disable,
        "edit" => InstructionsCommand::Edit,
        _ => {
            return Err(ParseError::InvalidArgument {
                command: "instructions",
                value: rest.to_string(),
                expected: "enable, disable, or edit",
            })
        }
    };
    Ok(Command::Instructions(action))
}

fn parse_mcp(rest: &str) -> std::result::Result<Command, ParseError> {
    let (action, target) = match rest.split_once(char::is_whitespace) {
        Some((action, target)) => (action, target.trim()),
        None => (rest, ""),
    };
    let target = (!target.is_empty()).then(|| target.to_string());

    let command = match action.to_ascii_lowercase().as_str() {
        "" | "list" => McpCommand::List,
        "add" => McpCommand::Add,
        "check" => McpCommand::Check(target),
        "reconnect" => McpCommand::Reconnect(required_mcp_name(target, "reconnect")?),
        "login" | "reauth" => McpCommand::Login(required_mcp_name(target, "login")?),
        "logout" => McpCommand::Logout(required_mcp_name(target, "logout")?),
        "remove" | "delete" => McpCommand::Remove(required_mcp_name(target, "remove")?),
        _ => {
            return Err(ParseError::InvalidArgument {
                command: "mcp",
                value: action.to_string(),
                expected: "list, add, check, reconnect, login, reauth, logout, remove, or delete",
            });
        }
    };
    Ok(Command::Mcp(command))
}

fn required_mcp_name(
    target: Option<String>,
    action: &'static str,
) -> std::result::Result<String, ParseError> {
    target.ok_or(ParseError::MissingArgument {
        command: "mcp",
        expected: match action {
            "reconnect" => "a server name after reconnect",
            "login" => "a server name after login or reauth",
            "logout" => "a server name after logout",
            "remove" => "a server name after remove or delete",
            _ => "a server name",
        },
    })
}

/// Command names offered for completion, in display order.
pub const COMMAND_NAMES: &[&str] = &[
    "/help",
    "/agent",
    "/model",
    "/effort",
    "/default",
    "/mode",
    "/backup",
    "/telegram",
    "/usage",
    "/status",
    "/update",
    "/agents",
    "/skills",
    "/instructions",
    "/thinking",
    "/mcp",
    "/context",
    "/compact",
    "/resume",
    "/new",
    "/clear-history",
    "/children",
    "/parent",
    "/delegate",
    "/queue",
    "/cancel",
    "/config",
    "/doctor",
    "/quit",
];

/// Argument-bearing completions for commands with a fixed subcommand set.
const SUBCOMMAND_COMPLETIONS: &[&str] = &[
    "/default clear",
    "/default configure",
    "/default current",
    "/update check",
    "/update force",
    "/update install",
    "/mcp check",
    "/mcp add",
    "/mcp delete",
    "/mcp list",
    "/mcp login",
    "/mcp logout",
    "/mcp reauth",
    "/mcp reconnect",
    "/mcp remove",
    "/telegram allow",
    "/telegram connect",
    "/telegram link",
    "/telegram remove",
    "/telegram reset",
    "/telegram setup",
    "/telegram status",
    "/thinking hide",
    "/thinking show",
    "/thinking toggle",
    "/instructions disable",
    "/instructions edit",
    "/instructions enable",
];

/// Completion candidates for a partially typed command.
pub fn complete(prefix: &str) -> Vec<&'static str> {
    // A multi-line message is prose, not a command, even if it opens with a
    // slash. Trimming first would hide the newline and offer commands anyway.
    if prefix.contains('\n') {
        return Vec::new();
    }
    // Preserve a trailing space: it is the signal that the user has finished the
    // command name and wants its fixed subcommands (for example `/update `).
    let prefix = prefix.trim_start();
    if !prefix.starts_with('/') {
        return Vec::new();
    }
    // A pasted file path opens with a slash too. The list is still useful for a
    // bare `/`, but it gets out of the way as soon as the text stops looking like
    // a command name.
    if prefix.len() > 1 && command_name(prefix).is_none() {
        return Vec::new();
    }
    if prefix.contains(char::is_whitespace) {
        return SUBCOMMAND_COMPLETIONS
            .iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .copied()
            .collect();
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
            usage: "/default [configure|current|clear]",
            detail: "configure the exact CLI/model used for new conversations",
        },
        HelpEntry {
            usage: "/mode [id]",
            detail: "switch execution mode (Shift+Tab cycles): full, plan, accept-edits",
        },
        HelpEntry {
            usage: "/backup [id [model]|none]",
            detail: "pick the CLI and model to continue on if this one runs out of quota",
        },
        HelpEntry {
            usage: "/telegram [status|setup|connect|link|allow|remove|reset]",
            detail: "set up, inspect, or remove phone access; reset remains an alias",
        },
        HelpEntry {
            usage: "/usage",
            detail: "show exact token counts reported by the last CLI turn",
        },
        HelpEntry {
            usage: "/status",
            detail: "show current conversation, selection, context, run, and queue state",
        },
        HelpEntry {
            usage: "/update [check|install|force]",
            detail: "check for updates, or exit and update Argo directly",
        },
        HelpEntry {
            usage: "/agents",
            detail: "browse CLIs; Enter switches, Space sets default, Delete clears it",
        },
        HelpEntry {
            usage: "/skills",
            detail: "list skills available to every agent",
        },
        HelpEntry {
            usage: "/instructions [enable|disable|edit]",
            detail: "manage opt-in project instructions captured from durable user directives",
        },
        HelpEntry {
            usage: "/thinking [show|hide|toggle]",
            detail: "show, hide, or toggle agent thinking and tool activity in the transcript",
        },
        HelpEntry {
            usage: "/mcp [list|check [name]]",
            detail: "list MCP servers or check their connection state",
        },
        HelpEntry {
            usage: "/mcp add",
            detail: "guided setup for local, remote, imported, OAuth, bearer-token, or header-auth MCP servers",
        },
        HelpEntry {
            usage: "/mcp reconnect <name>",
            detail: "disconnect and reconnect an MCP server",
        },
        HelpEntry {
            usage: "/mcp login|reauth <name>",
            detail: "authenticate an MCP server again",
        },
        HelpEntry {
            usage: "/mcp logout <name>",
            detail: "clear an MCP server's authentication",
        },
        HelpEntry {
            usage: "/mcp remove|delete <name>",
            detail: "delete an MCP server from configuration",
        },
        HelpEntry {
            usage: "/context",
            detail: "show exactly what your next message will send",
        },
        HelpEntry {
            usage: "/compact",
            detail: "fold this conversation into a summary to free context; the transcript is kept",
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
            usage: "/clear-history",
            detail: "delete stored chats in this workspace and start fresh",
        },
        HelpEntry {
            usage: "/children",
            detail: "inspect delegated conversations; Esc returns to the parent",
        },
        HelpEntry {
            usage: "/parent",
            detail: "return from a directly opened child chat to its parent",
        },
        HelpEntry {
            usage: "/delegate <agent> <task>",
            detail: "hand a task to a different CLI as a subagent",
        },
        HelpEntry {
            usage: "/queue",
            detail: "review messages waiting to send; Del or Ctrl+D drops the highlighted one",
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
    fn a_dragged_file_path_is_a_message_not_a_command() {
        // Dropping a screenshot on the composer pastes an absolute path; it used
        // to come back as "unknown command /var/folders/…" instead of reaching
        // the agent.
        let dropped = "/var/folders/yf/T/TemporaryItems/Screenshot\\ 2026-08-05.png";
        assert!(!is_command(dropped));
        assert!(complete(dropped).is_empty());

        assert!(!is_command(
            "/Users/matic/WORK/agentmux/README.md explain this"
        ));
        assert!(!is_command("/tmp/a.png /tmp/b.png compare these"));

        // Real commands, and the bare slash that opens the list, are untouched.
        assert!(is_command("/model opus"));
        assert!(!complete("/").is_empty());
        assert!(!complete("/mod").is_empty());
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
        assert_eq!(parse("/back").expect("parse"), Command::Parent);
        assert_eq!(parse("/?").expect("parse"), Command::Help);
        assert_eq!(
            parse("/reasoning high").expect("parse"),
            Command::Effort(Some("high".into()))
        );
        assert_eq!(
            parse("/conversations").expect("parse"),
            Command::Resume(None)
        );
        assert_eq!(
            parse("/THINKING SHOW").expect("parse"),
            Command::Thinking(ThinkingCommand::Show)
        );
    }

    #[test]
    fn telegram_help_and_completions_match_the_guided_actions() {
        let completions = complete("/telegram ");
        for action in [
            "status", "setup", "connect", "link", "allow", "remove", "reset",
        ] {
            let expected = format!("/telegram {action}");
            assert!(
                completions.iter().any(|candidate| *candidate == expected),
                "missing {action}: {completions:?}"
            );
        }
        let entry = help()
            .into_iter()
            .find(|entry| entry.usage.starts_with("/telegram"))
            .expect("Telegram help");
        assert!(!entry.usage.contains("start"), "{}", entry.usage);
        assert!(!entry.usage.contains("stop"), "{}", entry.usage);
    }

    #[test]
    fn thinking_defaults_to_toggle_and_accepts_each_visibility_action() {
        assert_eq!(
            parse("/thinking").expect("parse"),
            Command::Thinking(ThinkingCommand::Toggle)
        );
        assert_eq!(
            parse("/thinking toggle").expect("parse"),
            Command::Thinking(ThinkingCommand::Toggle)
        );
        assert_eq!(
            parse("/thinking show").expect("parse"),
            Command::Thinking(ThinkingCommand::Show)
        );
        assert_eq!(
            parse("/thinking hide").expect("parse"),
            Command::Thinking(ThinkingCommand::Hide)
        );
    }

    #[test]
    fn instructions_are_opt_in_and_expose_only_enable_disable_and_edit() {
        assert_eq!(
            parse("/instructions").expect("parse"),
            Command::Instructions(InstructionsCommand::Menu)
        );
        assert_eq!(
            parse("/instructions enable").expect("parse"),
            Command::Instructions(InstructionsCommand::Enable)
        );
        assert_eq!(
            parse("/instructions disable").expect("parse"),
            Command::Instructions(InstructionsCommand::Disable)
        );
        assert_eq!(
            parse("/instructions edit").expect("parse"),
            Command::Instructions(InstructionsCommand::Edit)
        );
        assert!(parse("/instructions show").is_err());
    }

    #[test]
    fn default_configuration_is_explicit_and_clearable() {
        assert_eq!(
            parse("/default").expect("parse"),
            Command::Default(DefaultCommand::Configure)
        );
        assert_eq!(
            parse("/default current").expect("parse"),
            Command::Default(DefaultCommand::Current)
        );
        assert_eq!(
            parse("/default clear").expect("parse"),
            Command::Default(DefaultCommand::Clear)
        );
        assert!(parse("/default codex").is_err());
    }

    #[test]
    fn update_can_check_install_or_force_a_reinstall() {
        assert_eq!(
            parse("/update").expect("parse"),
            Command::Update(UpdateCommand::Check)
        );
        assert_eq!(
            parse("/update install").expect("parse"),
            Command::Update(UpdateCommand::Install)
        );
        assert_eq!(
            parse("/upgrade force").expect("parse"),
            Command::Update(UpdateCommand::Force)
        );
        assert!(parse("/update maybe").is_err());
        assert!(complete("/update ").contains(&"/update install"));
    }

    #[test]
    fn thinking_rejects_unknown_actions_with_guidance() {
        let error = parse("/thinking sometimes").expect_err("must fail");
        assert_eq!(
            error,
            ParseError::InvalidArgument {
                command: "thinking",
                value: "sometimes".into(),
                expected: "show, hide, or toggle",
            }
        );
        assert!(error.to_string().contains("show, hide, or toggle"));
    }

    #[test]
    fn mcp_bare_and_list_forms_list_servers() {
        for line in ["/mcp", "/mcp list", "/MCP LIST"] {
            assert_eq!(parse(line).expect("parse"), Command::Mcp(McpCommand::List));
        }
    }

    #[test]
    fn mcp_add_opens_the_guided_flow() {
        assert_eq!(
            parse("/mcp add").expect("parse"),
            Command::Mcp(McpCommand::Add)
        );
        assert!(complete("/mcp a").contains(&"/mcp add"));
    }

    #[test]
    fn parses_mcp_inspection_and_lifecycle_actions() {
        assert_eq!(
            parse("/mcp check").expect("parse"),
            Command::Mcp(McpCommand::Check(None))
        );
        assert_eq!(
            parse("/mcp check github").expect("parse"),
            Command::Mcp(McpCommand::Check(Some("github".into())))
        );
        assert_eq!(
            parse("/mcp reconnect github").expect("parse"),
            Command::Mcp(McpCommand::Reconnect("github".into()))
        );
        assert_eq!(
            parse("/mcp login github").expect("parse"),
            Command::Mcp(McpCommand::Login("github".into()))
        );
        assert_eq!(
            parse("/mcp logout github").expect("parse"),
            Command::Mcp(McpCommand::Logout("github".into()))
        );
        assert_eq!(
            parse("/mcp remove github").expect("parse"),
            Command::Mcp(McpCommand::Remove("github".into()))
        );
    }

    #[test]
    fn mcp_aliases_map_to_their_canonical_operations() {
        assert_eq!(
            parse("/mcp reauth github").expect("parse"),
            Command::Mcp(McpCommand::Login("github".into()))
        );
        assert_eq!(
            parse("/mcp delete github").expect("parse"),
            Command::Mcp(McpCommand::Remove("github".into()))
        );
    }

    #[test]
    fn mcp_mutations_require_a_server_name() {
        for action in ["reconnect", "login", "reauth", "logout", "remove", "delete"] {
            let line = format!("/mcp {action}");
            let error = parse(&line).expect_err("must fail without a server name");
            assert!(
                matches!(error, ParseError::MissingArgument { command: "mcp", .. }),
                "unexpected error for {line}: {error}"
            );
            assert!(error.to_string().contains("server name"));
        }
    }

    #[test]
    fn mcp_rejects_unknown_actions_with_the_valid_set() {
        let error = parse("/mcp restart github").expect_err("must fail");
        assert!(matches!(
            error,
            ParseError::InvalidArgument {
                command: "mcp",
                ref value,
                ..
            } if value == "restart"
        ));
        let message = error.to_string();
        assert!(message.contains("reconnect"));
        assert!(message.contains("reauth"));
        assert!(message.contains("delete"));
    }

    #[test]
    fn parses_delegate_into_agent_and_task() {
        assert_eq!(
            parse("/delegate example-agent review my changes").expect("parse"),
            Command::Delegate {
                agent: "example-agent".into(),
                task: "review my changes".into()
            }
        );
    }

    #[test]
    fn delegate_without_a_task_is_rejected_with_guidance() {
        let error = parse("/delegate").expect_err("must fail");
        assert!(error.to_string().contains("/delegate <agent> <task>"));
        let error = parse("/delegate example-agent").expect_err("must fail");
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
    fn completion_offers_thinking_and_mcp_subcommands() {
        assert_eq!(complete("/thinking sh"), vec!["/thinking show"]);
        assert_eq!(
            complete("/mcp re"),
            vec!["/mcp reauth", "/mcp reconnect", "/mcp remove"]
        );
        assert_eq!(complete("/mcp del"), vec!["/mcp delete"]);
        // After the fixed action, the remainder is a user-defined server name.
        assert!(complete("/mcp login github").is_empty());
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
                "/delegate" => "/delegate example-agent do it".to_string(),
                other => other.to_string(),
            };
            assert!(parse(&line).is_ok(), "{name} must parse");
        }
    }

    #[test]
    fn the_queue_review_command_has_the_obvious_aliases() {
        for line in ["/queue", "/queued", "/pending"] {
            assert_eq!(parse(line).expect("parse"), Command::Queue, "{line}");
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

    #[test]
    fn help_documents_thinking_and_every_mcp_operation() {
        let entries = help();
        let usages: Vec<&str> = entries.iter().map(|entry| entry.usage).collect();
        assert!(usages.contains(&"/thinking [show|hide|toggle]"));
        for operation in [
            "list",
            "check",
            "reconnect",
            "login",
            "reauth",
            "logout",
            "remove",
            "delete",
        ] {
            assert!(
                usages
                    .iter()
                    .any(|usage| usage.starts_with("/mcp") && usage.contains(operation)),
                "/help does not document /mcp {operation}"
            );
        }
    }
}
