//! Argo runtime layer.
//!
//! Adapters are declarative values ([`def::RuntimeDef`]); the engine in this
//! crate performs detection, prompt staging, and argv construction uniformly for
//! all of them. Adding a CLI means adding a definition, not a code path.

pub mod def;
pub mod defs;
pub mod detect;
pub mod exec;
pub mod registry;
pub mod staging;
pub mod stream;
pub mod update;

pub use def::{AgentInfo, AuthProbe, InvocationContext, ModelProbe, RuntimeDef, DEFAULT_MODEL_ID};
pub use detect::{detect_all, detect_one, detect_version, discover_all_lightweight, discover_one};
pub use exec::{execute, CancelToken, ExecOutcome, ExecRequest};
pub use registry::{find, ids, require, validate, ADAPTERS};
pub use staging::StagedPrompt;
pub use stream::{CollectingSink, StreamSink, TerminalOutcome};
