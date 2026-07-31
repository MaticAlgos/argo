//! The adapter contract.
//!
//! An adapter is a plain data value, not a trait implementation. It declares
//! which binary to probe, how to build one turn's argv, how the CLI streams back,
//! and what it is capable of. A shared engine reads those fields and performs
//! detection, launching, parsing, and cancellation identically for every agent.
//!
//! Adding a CLI is therefore a new [`RuntimeDef`] plus a registry entry — no new
//! code path, unless the CLI speaks a wire format Argo has never seen.

use argo_core::runtime::{AgentCapabilities, ModelOption, ReasoningOption};
use serde::{Deserialize, Serialize};

/// Everything an adapter needs to know about one turn in order to build argv.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvocationContext {
    /// Resolved model id, when the user selected one.
    pub model: Option<String>,
    /// Reasoning effort, when the model supports it.
    pub reasoning: Option<String>,
    /// Composed prompt, for adapters whose CLI accepts it only as an argument.
    pub prompt: Option<String>,
    /// Path to the staged prompt file, for file-delivery adapters.
    pub prompt_file: Option<String>,
    /// Upstream session handle to continue, when resuming.
    pub resume_session: Option<String>,
    /// Session id Argo mints for a fresh session, when the CLI lets Argo choose.
    ///
    /// Specifying the id is more reliable than waiting to capture one: if the
    /// stream never discloses it, resume would otherwise be impossible.
    pub new_session: Option<String>,
    /// Absolute workspace root the child runs in.
    pub cwd: String,
    /// Additional absolute roots the CLI should be allowed to read.
    pub extra_dirs: Vec<String>,
    /// Path to a generated MCP configuration file, when injection uses one.
    pub mcp_config: Option<String>,
    /// Capability flags observed in the installed binary's help output.
    ///
    /// Lets an arg builder avoid passing a flag an older build would reject.
    pub help_flags: Vec<String>,
    /// Execution mode requested for this turn.
    pub mode: argo_core::mode::AgentMode,
}

impl InvocationContext {
    /// True when the installed CLI advertises `flag` in its help output.
    pub fn supports_flag(&self, flag: &str) -> bool {
        self.help_flags.iter().any(|f| f == flag)
    }

    /// The model id, when it is a concrete selection rather than the CLI default.
    ///
    /// `default` is Argo's sentinel for "leave the CLI's own configuration
    /// authoritative", so it must never be passed through as `--model default`.
    pub fn concrete_model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .filter(|m| !m.is_empty() && *m != DEFAULT_MODEL_ID)
    }
}

/// Sentinel model id meaning "use whatever the CLI is configured for".
pub const DEFAULT_MODEL_ID: &str = "default";

/// Builds the argv (excluding the binary itself) for one turn.
type BuildArgsFn = fn(&InvocationContext) -> Vec<String>;

/// Extracts a durable session id from adapter-owned state after a turn.
pub type CaptureSessionFn = fn(&InvocationContext, i64) -> Option<String>;

/// Parses a model-discovery command's stdout into selectable models.
type ParseModelsFn = fn(&str) -> Vec<ModelOption>;

/// Extracts per-model reasoning levels from the same discovery output.
///
/// Reasoning levels are a property of the model rather than the CLI, so a single
/// adapter-wide list would either hide levels or offer invalid ones.
type ParseReasoningFn = fn(&str) -> Vec<(String, Vec<ReasoningOption>)>;

/// A model-discovery probe.
#[derive(Debug, Clone)]
pub struct ModelProbe {
    /// Arguments that list models.
    pub args: &'static [&'static str],
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
    /// Parser for the command's stdout.
    pub parse: ParseModelsFn,
    /// Optional parser for per-model reasoning levels.
    pub parse_reasoning: Option<ParseReasoningFn>,
}

/// An authentication probe, for CLIs that expose one.
#[derive(Debug, Clone)]
pub struct AuthProbe {
    /// Arguments that report login status.
    pub args: &'static [&'static str],
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
}

/// One CLI adapter, declared entirely as data.
#[derive(Debug, Clone)]
pub struct RuntimeDef {
    /// Stable adapter id, unique across the registry.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Executable probed on PATH.
    pub bin: &'static str,
    /// Alternate executable names, tried in order.
    pub fallback_bins: &'static [&'static str],
    /// Arguments that print a version.
    pub version_args: &'static [&'static str],
    /// Arguments that print help, used to detect optional flags.
    pub help_args: &'static [&'static str],
    /// Optional model-discovery probe.
    pub model_probe: Option<ModelProbe>,
    /// Models offered when discovery is unavailable.
    pub fallback_models: &'static [(&'static str, &'static str)],
    /// Reasoning levels this CLI accepts.
    pub reasoning_options: &'static [(&'static str, &'static str)],
    /// Optional authentication probe.
    pub auth_probe: Option<AuthProbe>,
    /// Builds argv for a turn.
    pub build_args: BuildArgsFn,
    /// Optional post-run discovery for CLIs that persist session ids out of band.
    pub capture_session: Option<CaptureSessionFn>,
    /// Declared capabilities.
    pub capabilities: AgentCapabilities,
    /// Where to install the CLI, shown when it is missing.
    pub install_url: &'static str,
}

impl RuntimeDef {
    /// Executable names to try, preferred first.
    pub fn candidate_bins(&self) -> Vec<&'static str> {
        let mut out = vec![self.bin];
        out.extend_from_slice(self.fallback_bins);
        out
    }

    /// Fallback models as owned options.
    pub fn fallback_model_options(&self) -> Vec<ModelOption> {
        self.fallback_models
            .iter()
            .map(|(id, label)| ModelOption::labeled(*id, *label))
            .collect()
    }

    /// Reasoning options as owned values.
    pub fn reasoning_option_values(&self) -> Vec<ReasoningOption> {
        self.reasoning_options
            .iter()
            .map(|(id, label)| ReasoningOption {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect()
    }

    /// Builds argv for a turn.
    pub fn args_for(&self, context: &InvocationContext) -> Vec<String> {
        (self.build_args)(context)
    }
}

/// Detection outcome for one adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Adapter id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// True when the binary was found and could be executed.
    pub available: bool,
    /// Resolved absolute path to the executable.
    pub path: Option<String>,
    /// Version string, when the CLI reported one.
    pub version: Option<String>,
    /// Authentication state, only set when the adapter declares a probe.
    pub authenticated: Option<bool>,
    /// Selectable models.
    pub models: Vec<ModelOption>,
    /// True when `models` came from a live probe rather than fallbacks.
    pub models_live: bool,
    /// Reasoning levels that apply to every model this adapter offers.
    pub reasoning: Vec<ReasoningOption>,
    /// Reasoning levels for specific models, when the CLI reports them per model.
    ///
    /// Consulted before `reasoning`, so `/effort` offers exactly what the selected
    /// model accepts.
    pub model_reasoning: Vec<(String, Vec<ReasoningOption>)>,
    /// Declared capabilities.
    pub capabilities: AgentCapabilities,
    /// Actionable diagnostics, such as a missing binary or a failed login.
    pub diagnostics: Vec<String>,
    /// Install URL, shown when unavailable.
    pub install_url: String,
}

impl AgentInfo {
    /// Builds an "unavailable" result carrying an actionable reason.
    ///
    /// Capability limitations are included even when the CLI is missing, so a
    /// user comparing agents in the picker learns what each one gives up before
    /// deciding to install it.
    pub fn unavailable(def: &RuntimeDef, reason: impl Into<String>) -> Self {
        let mut diagnostics = vec![reason.into()];
        diagnostics.extend(capability_diagnostics(def));
        Self {
            id: def.id.to_string(),
            name: def.name.to_string(),
            available: false,
            path: None,
            version: None,
            authenticated: None,
            models: def.fallback_model_options(),
            models_live: false,
            reasoning: def.reasoning_option_values(),
            model_reasoning: Vec::new(),
            capabilities: def.capabilities.clone(),
            diagnostics,
            install_url: def.install_url.to_string(),
        }
    }

    /// Reasoning levels applicable to `model`.
    ///
    /// Falls back to the adapter-wide list when the CLI does not report per-model
    /// levels, and to nothing when the adapter has no reasoning concept at all.
    pub fn reasoning_for(&self, model: Option<&str>) -> &[ReasoningOption] {
        if let Some(model) = model {
            if let Some((_, levels)) = self.model_reasoning.iter().find(|(slug, _)| slug == model) {
                return levels;
            }
        }
        &self.reasoning
    }
}

/// Diagnostics derived purely from declared capabilities.
///
/// These are facts about the CLI's interface rather than its installation state,
/// so they apply whether or not the binary was found.
pub fn capability_diagnostics(def: &RuntimeDef) -> Vec<String> {
    let mut out = Vec::new();
    if !def.capabilities.native_resume {
        out.push(
            "this CLI has no resumable session, so every turn is reseeded with conversation context"
                .to_string(),
        );
    }
    if !def.capabilities.stream_format.has_structured_tool_events() {
        out.push(
            "this CLI emits plain text, so per-tool events are unavailable and file changes are reconciled after the run"
                .to_string(),
        );
    }
    if !def.capabilities.can_delegate() {
        out.push(
            "this CLI cannot host Argo's delegation tools, so it can be a subagent target but cannot spawn subagents"
                .to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_sentinel_is_never_treated_as_concrete() {
        // Passing `--model default` would be rejected by every CLI Argo targets.
        let mut ctx = InvocationContext {
            model: Some(DEFAULT_MODEL_ID.into()),
            ..Default::default()
        };
        assert_eq!(ctx.concrete_model(), None);
        ctx.model = Some(String::new());
        assert_eq!(ctx.concrete_model(), None);
        ctx.model = Some("gpt-5.6".into());
        assert_eq!(ctx.concrete_model(), Some("gpt-5.6"));
        ctx.model = None;
        assert_eq!(ctx.concrete_model(), None);
    }

    #[test]
    fn help_flag_detection_gates_optional_arguments() {
        let ctx = InvocationContext {
            help_flags: vec!["--dangerously-skip-permissions".into()],
            ..Default::default()
        };
        assert!(ctx.supports_flag("--dangerously-skip-permissions"));
        assert!(!ctx.supports_flag("--trust"));
    }
}
