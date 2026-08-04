//! Telegram transport and formatting for Argo's remote bridge.
//!
//! This crate deliberately knows nothing about the daemon: it converts agent
//! output into something Telegram will accept, and carries messages in and out.
//! The bridge that joins the two lives in `argo-daemon`, which keeps the
//! formatting rules testable without a daemon, a token, or a network.

pub mod bot;
pub mod config;
pub mod markdown_v2;
pub mod qr;
pub mod render;
pub mod split;

pub use bot::{
    Bot, BotIdentity, CallbackQuery, IncomingMessage, KeyboardRow, ParseMode, Update,
    MAX_CALLBACK_DATA,
};
pub use config::TelegramConfig;
pub use markdown_v2::to_markdown_v2;
pub use split::{
    plain_text, safe_message_chunk, split_message, split_message_safe, MessageChunk,
    MAX_MESSAGE_CHARS,
};
