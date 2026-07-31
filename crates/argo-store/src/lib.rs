//! Argo canonical store.
//!
//! SQLite in WAL mode is the authority for conversations, messages, runs,
//! normalized events, and upstream session handles. Upstream CLI session stores
//! are treated as reusable caches; if one disappears, Argo can always rebuild
//! context from these rows.

pub mod messages;
pub mod runs;
pub mod schema;
pub mod sessions;
pub mod store;

pub use messages::NewMessage;
pub use runs::{NewRun, Run};
pub use schema::SCHEMA_VERSION;
pub use store::{Conversation, Store};
