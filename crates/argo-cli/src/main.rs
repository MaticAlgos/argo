//! The `argo` command-line entry point.
//!
//! Subcommands are thin: they connect to the daemon (starting it if needed) and
//! render its replies. The daemon owns all state, so two terminals running `argo`
//! observe the same conversations.

mod client;

use argo_core::error::{ArgoError, Result};
use argo_core::ArgoPaths;
use clap::{Parser, Subcommand};

/// Argo: one conversation across many coding-agent CLIs.
#[derive(Debug, Parser)]
#[command(name = "argo", version, about, long_about = None)]
struct Cli {
    /// Override the data directory.
    #[arg(long, global = true)]
    data_dir: Option<String>,

    /// Stop Argo and remove this installed executable; conversations are preserved.
    #[arg(long)]
    uninstall: bool,

    /// Resume a conversation directly by its full id.
    #[arg(long)]
    resume: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// MCP management actions.
#[derive(Debug, Subcommand)]
enum McpAction {
    /// List configured servers.
    List,
    /// Add a server. Use a URL for a remote one, or `--` then a command for a local one.
    ///
    /// Examples:
    ///   argo mcp add volrix --url https://mcp.volrix.ai/mcp
    ///   argo mcp add everything -- npx -y @modelcontextprotocol/server-everything
    Add {
        /// Unique name; becomes the tool prefix the agent sees.
        name: String,
        /// Endpoint for a remote server.
        #[arg(long)]
        url: Option<String>,
        /// Header as `Name: value`, repeatable. Values may use `{env:VAR}`.
        #[arg(long = "header")]
        headers: Vec<String>,
        /// Command and arguments for a local server.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Import servers already configured in other agents' config files.
    Import {
        /// Import without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Probe each server and report why it is not working.
    Check,
    /// Log in to an OAuth-protected server, once, for every agent.
    Login {
        /// Server name as shown by `argo mcp list`.
        name: String,
    },
    /// Forget stored credentials for a server.
    Logout {
        /// Server name.
        name: String,
    },
    /// Remove a server.
    Remove {
        /// Server name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the interactive terminal UI (the default).
    Tui {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
    },
    /// Run the daemon in the foreground.
    Daemon,
    /// Serve Argo's delegation tools over MCP on stdio.
    ///
    /// Launched by a coding agent, not by a user: this is how one CLI delegates to
    /// another.
    McpServer,
    /// Report environment, database, and adapter status.
    Doctor,
    /// List detected coding-agent CLIs.
    Agents {
        /// Re-probe instead of using the cached inventory.
        #[arg(long)]
        refresh: bool,
    },
    /// List conversations in a workspace.
    Chats {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
    },
    /// Delete stored conversation history.
    ClearHistory {
        /// Clear every workspace instead of only the selected root.
        #[arg(long)]
        all: bool,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
    },
    /// Create a conversation without sending anything.
    New {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
        /// Optional title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Set the agent, model, or reasoning level for a conversation's next turn.
    Select {
        /// Conversation id.
        conversation_id: String,
        /// Agent to use.
        #[arg(long)]
        agent: Option<String>,
        /// Model to use.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Set the execution mode applied to a conversation's next turn.
    Mode {
        /// Conversation id.
        conversation_id: String,
        /// Mode: full, plan, accept-edits, or read-only. Omit to show the current one.
        mode: Option<String>,
    },
    /// Manage MCP servers shared with every agent.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// List skills Argo discovered for a workspace.
    Skills {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
    },
    /// Show a conversation's messages.
    Show {
        /// Conversation id.
        conversation_id: String,
    },
    /// Send one message and stream the reply.
    Send {
        /// Conversation id. A new conversation is created when omitted.
        #[arg(long)]
        conversation_id: Option<String>,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
        /// Agent to use for this turn.
        #[arg(long)]
        agent: Option<String>,
        /// Model to use for this turn.
        #[arg(long)]
        model: Option<String>,
        /// The message.
        message: String,
    },
    /// Delegate exploratory work to another installed coding CLI.
    ///
    /// Inside an Argo-managed agent turn, parent ids are read from the environment.
    Delegate {
        /// Target adapter id, for example codex, claude, or kiro.
        agent: String,
        /// Optional model for the delegated CLI.
        #[arg(long)]
        model: Option<String>,
        /// Explicit parent conversation; normally supplied by Argo's environment.
        #[arg(long)]
        parent_conversation_id: Option<String>,
        /// Explicit host run; normally supplied by Argo's environment.
        #[arg(long)]
        parent_run_id: Option<String>,
        /// Self-contained task; unquoted remaining words are joined with spaces.
        #[arg(required = true, trailing_var_arg = true)]
        task: Vec<String>,
    },
    /// Show the exact context the next turn would send.
    Context {
        /// Conversation id.
        conversation_id: String,
        /// Prompt to preview.
        prompt: String,
    },
    /// Stop the running daemon.
    Stop,
    /// Check for a newer Argo build and install it from GitHub.
    Update {
        /// Only report whether an update is available.
        #[arg(long)]
        check: bool,
        /// Reinstall the published build even when the version is unchanged.
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    if let Some(dir) = &cli.data_dir {
        // Set before any path resolution so every component agrees on the root.
        std::env::set_var(argo_core::paths::DATA_DIR_ENV, dir);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("argo: cannot start async runtime: {error}");
            std::process::exit(1);
        }
    };

    let result = runtime.block_on(run(cli));

    if let Err(error) = result {
        eprintln!("argo: {error}");
        std::process::exit(exit_code(&error));
    }
}

/// Maps an error onto a stable process exit code.
fn exit_code(error: &ArgoError) -> i32 {
    match error {
        ArgoError::NotFound { .. } => 4,
        ArgoError::AgentUnavailable { .. } => 5,
        ArgoError::Cancelled => 130,
        ArgoError::Timeout(_) => 124,
        _ => 1,
    }
}

async fn run(cli: Cli) -> Result<()> {
    let paths = ArgoPaths::resolve()?;

    if cli.uninstall {
        if cli.resume.is_some() || cli.command.is_some() {
            return Err(ArgoError::Invalid(
                "--uninstall cannot be combined with --resume or a subcommand".into(),
            ));
        }
        return client::uninstall(&paths).await;
    }

    // --resume takes priority over a subcommand when no subcommand is given.
    if let Some(resume_id) = cli.resume {
        if cli.command.is_some() {
            return Err(ArgoError::Invalid(
                "--resume cannot be combined with a subcommand".into(),
            ));
        }
        return argo_tui::run_with_conversation(&paths, resume_id).await;
    }

    match cli.command.unwrap_or(Command::Tui { root: None }) {
        Command::Tui { root } => {
            let root = match root {
                Some(root) => root,
                None => std::env::current_dir()
                    .map_err(|e| ArgoError::Io(format!("resolve current directory: {e}")))?
                    .to_string_lossy()
                    .to_string(),
            };
            argo_tui::run(&paths, root).await
        }
        Command::Daemon => run_daemon(paths).await,
        Command::McpServer => argo_daemon::mcp::serve_stdio(&paths).await,
        Command::Doctor => client::doctor(&paths).await,
        Command::Agents { refresh } => client::agents(&paths, refresh).await,
        Command::Chats { root } => client::chats(&paths, root).await,
        Command::ClearHistory { all, root } => client::clear_history(&paths, all, root).await,
        Command::New { root, title } => client::new_conversation(&paths, root, title).await,
        Command::Select {
            conversation_id,
            agent,
            model,
            reasoning,
        } => client::select(&paths, &conversation_id, agent, model, reasoning).await,
        Command::Mode {
            conversation_id,
            mode,
        } => client::mode(&paths, &conversation_id, mode).await,
        Command::Mcp { action } => match action {
            McpAction::List => client::mcp_list(&paths).await,
            McpAction::Add {
                name,
                url,
                headers,
                command,
            } => client::mcp_add(&paths, name, url, headers, command).await,
            McpAction::Import { yes } => client::mcp_import(&paths, yes).await,
            McpAction::Check => client::mcp_check(&paths).await,
            McpAction::Login { name } => client::mcp_login(&paths, &name).await,
            McpAction::Logout { name } => client::mcp_logout(&paths, &name).await,
            McpAction::Remove { name } => client::mcp_remove(&paths, &name).await,
        },
        Command::Skills { root } => client::skills(&paths, root).await,
        Command::Show { conversation_id } => client::show(&paths, &conversation_id).await,
        Command::Send {
            conversation_id,
            root,
            agent,
            model,
            message,
        } => client::send(&paths, conversation_id, root, agent, model, message).await,
        Command::Delegate {
            agent,
            model,
            parent_conversation_id,
            parent_run_id,
            task,
        } => {
            client::delegate(
                &paths,
                parent_conversation_id,
                parent_run_id,
                agent,
                model,
                task.join(" "),
            )
            .await
        }
        Command::Context {
            conversation_id,
            prompt,
        } => client::context(&paths, &conversation_id, &prompt).await,
        Command::Stop => client::stop(&paths).await,
        Command::Update { check, force } => client::update(check, force).await,
    }
}

/// Runs the daemon in the foreground until shutdown.
async fn run_daemon(paths: ArgoPaths) -> Result<()> {
    init_tracing();

    // The lock is acquired before binding so a second daemon fails fast with a
    // clear message instead of racing for the socket.
    let lock = argo_daemon::InstanceLock::acquire(paths.lock_file())?;
    let daemon = std::sync::Arc::new(argo_daemon::server::Daemon::bootstrap(paths).await?);

    let serving = argo_daemon::serve(std::sync::Arc::clone(&daemon), lock);

    tokio::select! {
        result = serving => result,
        _ = shutdown_signal() => {
            tracing::info!("received shutdown signal");
            Ok(())
        }
    }
}

/// Resolves when the process is asked to terminate.
async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                // Without SIGTERM handling, Ctrl-C alone still works.
                Err(_) => return interrupt.await.unwrap_or(()),
            };
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        interrupt.await.unwrap_or(());
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("ARGO_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // Logs go to stderr so stdout stays parseable for scripted use.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches conflicting flags and malformed arg definitions at test time
        // rather than on first user invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn send_requires_a_message() {
        let result = Cli::try_parse_from(["argo", "send"]);
        assert!(result.is_err(), "a message is mandatory");
    }

    #[test]
    fn send_parses_agent_and_model_overrides() {
        let cli = Cli::try_parse_from([
            "argo",
            "send",
            "--agent",
            "codex",
            "--model",
            "gpt-5.6",
            "fix the bug",
        ])
        .expect("parse");
        match cli.command.expect("command") {
            Command::Send {
                agent,
                model,
                message,
                ..
            } => {
                assert_eq!(agent.as_deref(), Some("codex"));
                assert_eq!(model.as_deref(), Some("gpt-5.6"));
                assert_eq!(message, "fix the bug");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_supports_check_and_force_modes() {
        let checked = Cli::try_parse_from(["argo", "update", "--check"]).expect("check parse");
        assert!(matches!(
            checked.command,
            Some(Command::Update {
                check: true,
                force: false
            })
        ));

        let forced = Cli::try_parse_from(["argo", "update", "--force"]).expect("force parse");
        assert!(matches!(
            forced.command,
            Some(Command::Update {
                check: false,
                force: true
            })
        ));
        assert!(Cli::try_parse_from(["argo", "update", "--check", "--force"]).is_err());
    }

    #[test]
    fn uninstall_is_an_explicit_top_level_flag() {
        let cli = Cli::try_parse_from(["argo", "--uninstall"]).expect("uninstall parse");
        assert!(cli.uninstall);
        assert!(cli.command.is_none());
    }

    #[test]
    fn delegate_accepts_a_multiword_task_and_optional_lineage() {
        let cli = Cli::try_parse_from([
            "argo",
            "delegate",
            "codex",
            "--model",
            "gpt-5.6-sol",
            "--parent-conversation-id",
            "conversation-1",
            "inspect",
            "the",
            "failure",
        ])
        .expect("parse");
        match cli.command.expect("command") {
            Command::Delegate {
                agent,
                model,
                parent_conversation_id,
                task,
                ..
            } => {
                assert_eq!(agent, "codex");
                assert_eq!(model.as_deref(), Some("gpt-5.6-sol"));
                assert_eq!(parent_conversation_id.as_deref(), Some("conversation-1"));
                assert_eq!(task.join(" "), "inspect the failure");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn no_subcommand_opens_the_interactive_ui() {
        // Bare `argo` should drop the user into the chat UI, not print a report.
        let cli = Cli::try_parse_from(["argo"]).expect("parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn tui_accepts_an_explicit_root() {
        let cli = Cli::try_parse_from(["argo", "tui", "--root", "/repo"]).expect("parse");
        match cli.command.expect("command") {
            Command::Tui { root } => assert_eq!(root.as_deref(), Some("/repo")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn data_dir_is_a_global_flag() {
        let cli = Cli::try_parse_from(["argo", "--data-dir", "/tmp/x", "agents"]).expect("parse");
        assert_eq!(cli.data_dir.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn exit_codes_are_distinct_and_conventional() {
        assert_eq!(exit_code(&ArgoError::not_found("run", "x")), 4);
        assert_eq!(exit_code(&ArgoError::Cancelled), 130);
        assert_eq!(exit_code(&ArgoError::Timeout(1)), 124);
        assert_eq!(exit_code(&ArgoError::Invalid("x".into())), 1);
    }

    #[test]
    fn resume_flag_opens_tui_with_conversation() {
        let cli = Cli::try_parse_from(["argo", "--resume", "abc-def-123"]).expect("parse");
        assert_eq!(cli.resume.as_deref(), Some("abc-def-123"));
        assert!(cli.command.is_none());
    }
}
