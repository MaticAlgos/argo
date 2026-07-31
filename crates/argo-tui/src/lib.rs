//! Argo interactive terminal UI.
//!
//! Argo owns the chat surface: the composer, the streamed transcript, the
//! agent/model pickers, and the slash commands. Child CLIs run headless through
//! structured transports rather than having their own TUI embedded, which is what
//! makes one conversation span several vendors.

pub mod app;
pub mod banner;
pub mod commands;
mod markdown;
pub mod render;
pub mod run;

pub use app::{App, EnterAction, LineKind, Overlay, PickerAction};
pub use commands::{parse, Command};
pub use run::run;
