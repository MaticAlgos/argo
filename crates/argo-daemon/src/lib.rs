//! Argo daemon.
//!
//! One per-user process owns the SQLite store, the adapter inventory, and every
//! child process. Clients (the TUI, and `argo` subcommands) talk to it over a
//! private Unix socket. Centralizing writes is what prevents two terminals from
//! interleaving partial turns into the same conversation.

pub mod engine;
pub mod lifecycle;
pub mod lock;
pub mod mcp;
pub mod protocol;
pub mod server;
pub mod telegram;

pub use engine::{run_turn, TurnOutcome, TurnRequest, STABLE_INSTRUCTIONS};
pub use lifecycle::{mismatched_daemon_protocol, stop_older_daemon};
pub use lock::InstanceLock;
pub use protocol::{ConversationSummary, MessageView, Request, Response};
pub use server::{serve, Daemon};
