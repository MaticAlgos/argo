//! Execution modes.
//!
//! Most coding CLIs expose some notion of "how much may you do without asking":
//! Claude has `--permission-mode`, Command Code has `--permission-mode` plus
//! `--plan`, Antigravity has `--mode`, OpenCode selects a `plan` agent, Codex
//! varies its sandbox, and Kiro switches mode over ACP.
//!
//! Argo models that as one vocabulary so a single keystroke means the same thing
//! everywhere, and each adapter translates it into its own flags. Where a CLI
//! cannot express a mode, that is declared rather than silently ignored — telling
//! a user they are in plan mode when the agent can still write files would be
//! worse than admitting the gap.

use serde::{Deserialize, Serialize};

/// How much authority the agent has for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    /// Full authority, matching Argo's default full-bypass posture.
    #[default]
    Full,
    /// Analyze and propose, but do not modify anything.
    Plan,
    /// Edit files freely, but do not run arbitrary commands unprompted.
    AcceptEdits,
    /// Read-only inspection.
    ReadOnly,
}

impl AgentMode {
    /// Short label for the status bar.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Full => "full access",
            Self::Plan => "plan",
            Self::AcceptEdits => "accept edits",
            Self::ReadOnly => "read only",
        }
    }

    /// One-line explanation of what the mode permits.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Full => "the agent may edit files and run commands without asking",
            Self::Plan => "the agent analyzes and proposes but does not change anything",
            Self::AcceptEdits => "the agent may edit files but not run arbitrary commands",
            Self::ReadOnly => "the agent may only read",
        }
    }

    /// Stable identifier used on the wire and in `/mode <id>`.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Plan => "plan",
            Self::AcceptEdits => "accept-edits",
            Self::ReadOnly => "read-only",
        }
    }

    /// Parses an identifier, accepting the spellings users actually type.
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "full" | "full-access" | "bypass" | "default" => Some(Self::Full),
            "plan" | "planning" => Some(Self::Plan),
            "accept-edits" | "acceptedits" | "edits" | "auto-accept" => Some(Self::AcceptEdits),
            "read-only" | "readonly" | "read" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// Instruction stating the mode's boundary in the prompt itself.
    ///
    /// A CLI flag alone is not enough: several agents have no mode flag, and a
    /// headless `-p` run may not honor an interactive permission mode. Stating the
    /// boundary in the prompt is what makes the mode hold everywhere.
    pub fn directive(&self) -> Option<&'static str> {
        match self {
            Self::Full => None,
            Self::Plan => Some(
                "## Mode: PLAN\n\
                 Do not modify anything. Do not create, edit, or delete files, and do not run \
                 commands that change state. Investigate by reading, then reply with a concrete \
                 plan: the files you would change, what each change does, and anything you are \
                 unsure about. Wait for approval before implementing.",
            ),
            Self::AcceptEdits => Some(
                "## Mode: EDIT ONLY\n\
                 You may create and edit files. Do not run build, test, install, deploy, or other \
                 state-changing commands unless the user explicitly asks. Reading and searching \
                 are fine.",
            ),
            Self::ReadOnly => Some(
                "## Mode: READ ONLY\n\
                 Do not modify anything and do not run commands that change state. Read, search, \
                 and explain only.",
            ),
        }
    }

    /// Cycle order used by the mode-switch keybinding.
    ///
    /// Deliberately short: cycling through four options with one key is already at
    /// the limit of what stays predictable.
    pub const CYCLE: &'static [Self] = &[Self::Full, Self::Plan, Self::AcceptEdits];

    /// The next mode in the cycle.
    pub fn next(&self) -> Self {
        let position = Self::CYCLE.iter().position(|m| m == self).unwrap_or(0);
        Self::CYCLE[(position + 1) % Self::CYCLE.len()]
    }

    /// True when the mode forbids modifying the workspace.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::Plan | Self::ReadOnly)
    }
}

/// Which modes an adapter can actually enforce.
///
/// Declared per adapter so the TUI offers only what the CLI honors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSupport {
    /// True when the adapter can be put into plan mode.
    pub plan: bool,
    /// True when the adapter can accept edits but withhold command execution.
    pub accept_edits: bool,
    /// True when the adapter can be restricted to reading.
    pub read_only: bool,
}

impl ModeSupport {
    /// An adapter that cannot express any mode beyond its default.
    pub const NONE: Self = Self {
        plan: false,
        accept_edits: false,
        read_only: false,
    };

    /// An adapter supporting plan and accept-edits.
    pub const PLAN_AND_EDITS: Self = Self {
        plan: true,
        accept_edits: true,
        read_only: false,
    };

    /// Adds Argo's portable prompt-enforced planning mode to native adapter
    /// capabilities. The conversation state and boundary directive belong to
    /// Argo; a verified CLI-native plan switch is an additional enforcement
    /// layer, not a prerequisite for using Shift+Tab.
    pub const fn with_argo_plan(mut self) -> Self {
        self.plan = true;
        self
    }

    /// True when `mode` can be honored.
    pub const fn supports(&self, mode: AgentMode) -> bool {
        match mode {
            AgentMode::Full => true,
            AgentMode::Plan => self.plan,
            AgentMode::AcceptEdits => self.accept_edits,
            AgentMode::ReadOnly => self.read_only,
        }
    }

    /// True when at least one non-default mode is available.
    pub const fn has_any(&self) -> bool {
        self.plan || self.accept_edits || self.read_only
    }

    /// Modes this adapter offers, in cycle order.
    pub fn available(&self) -> Vec<AgentMode> {
        let mut out = vec![AgentMode::Full];
        for mode in [AgentMode::Plan, AgentMode::AcceptEdits, AgentMode::ReadOnly] {
            if self.supports(mode) {
                out.push(mode);
            }
        }
        out
    }

    /// Next supported mode after `current`, skipping unsupported ones.
    ///
    /// Cycling must never land on a mode the CLI would ignore, or the status bar
    /// would claim a restriction that is not in force.
    pub fn next_supported(&self, current: AgentMode) -> AgentMode {
        let available = self.available();
        let position = available.iter().position(|m| *m == current).unwrap_or(0);
        available[(position + 1) % available.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip() {
        for mode in [
            AgentMode::Full,
            AgentMode::Plan,
            AgentMode::AcceptEdits,
            AgentMode::ReadOnly,
        ] {
            assert_eq!(AgentMode::parse(mode.id()), Some(mode));
        }
    }

    #[test]
    fn parsing_accepts_the_spellings_users_type() {
        assert_eq!(AgentMode::parse("PLAN"), Some(AgentMode::Plan));
        assert_eq!(AgentMode::parse("read_only"), Some(AgentMode::ReadOnly));
        assert_eq!(
            AgentMode::parse("auto-accept"),
            Some(AgentMode::AcceptEdits)
        );
        assert_eq!(AgentMode::parse(" default "), Some(AgentMode::Full));
        assert_eq!(AgentMode::parse("nonsense"), None);
    }

    #[test]
    fn plan_and_read_only_forbid_modification() {
        assert!(AgentMode::Plan.is_read_only());
        assert!(AgentMode::ReadOnly.is_read_only());
        assert!(!AgentMode::Full.is_read_only());
        assert!(!AgentMode::AcceptEdits.is_read_only());
    }

    #[test]
    fn the_cycle_returns_to_its_start() {
        let mut mode = AgentMode::Full;
        for _ in 0..AgentMode::CYCLE.len() {
            mode = mode.next();
        }
        assert_eq!(mode, AgentMode::Full);
    }

    #[test]
    fn cycling_skips_modes_the_adapter_cannot_honor() {
        // Claiming plan mode on a CLI that ignores it would misrepresent the
        // authority actually in force.
        let plan_only = ModeSupport {
            plan: true,
            accept_edits: false,
            read_only: false,
        };
        assert_eq!(plan_only.next_supported(AgentMode::Full), AgentMode::Plan);
        assert_eq!(
            plan_only.next_supported(AgentMode::Plan),
            AgentMode::Full,
            "must skip accept-edits"
        );
    }

    #[test]
    fn an_adapter_without_modes_stays_on_full() {
        assert_eq!(
            ModeSupport::NONE.next_supported(AgentMode::Full),
            AgentMode::Full
        );
        assert!(!ModeSupport::NONE.has_any());
        assert_eq!(ModeSupport::NONE.available(), vec![AgentMode::Full]);
    }

    #[test]
    fn argo_managed_plan_can_wrap_an_adapter_without_native_modes() {
        let support = ModeSupport::NONE.with_argo_plan();
        assert!(support.plan);
        assert!(!support.accept_edits);
        assert_eq!(support.available(), vec![AgentMode::Full, AgentMode::Plan]);
    }

    #[test]
    fn support_is_reported_per_mode() {
        let support = ModeSupport::PLAN_AND_EDITS;
        assert!(support.supports(AgentMode::Full));
        assert!(support.supports(AgentMode::Plan));
        assert!(support.supports(AgentMode::AcceptEdits));
        assert!(!support.supports(AgentMode::ReadOnly));
        assert!(support.has_any());
    }

    #[test]
    fn restrictive_modes_carry_a_prompt_directive() {
        // A flag alone does not hold: some CLIs have no mode flag, and a headless
        // run may ignore an interactive permission mode.
        assert!(AgentMode::Full.directive().is_none());
        for mode in [AgentMode::Plan, AgentMode::AcceptEdits, AgentMode::ReadOnly] {
            let directive = mode
                .directive()
                .expect("a restrictive mode must state its boundary");
            assert!(directive.starts_with("## Mode:"));
        }
        let plan = AgentMode::Plan.directive().expect("plan");
        assert!(plan.contains("Do not modify anything"));
        assert!(plan.contains("plan"));
        // Edit-only must permit edits while withholding command execution.
        let edits = AgentMode::AcceptEdits.directive().expect("edits");
        assert!(edits.contains("may create and edit files"));
        assert!(edits.contains("Do not run build"));
    }

    #[test]
    fn full_is_the_default_matching_argos_posture() {
        assert_eq!(AgentMode::default(), AgentMode::Full);
        assert!(AgentMode::Full.detail().contains("without asking"));
    }
}
