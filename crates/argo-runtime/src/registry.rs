//! The adapter registry.
//!
//! One array holds every shipped adapter. Lookup is by id, and a startup
//! invariant rejects duplicates: two adapters sharing an id would silently
//! shadow each other and route turns to the wrong CLI.

use crate::def::RuntimeDef;
use crate::defs::{
    antigravity::ANTIGRAVITY, claude::CLAUDE, codex::CODEX, command_code::COMMAND_CODE, grok::GROK,
    kiro::KIRO, opencode::OPENCODE,
};
use argo_core::error::{ArgoError, Result};

/// Every shipped adapter, in picker order.
pub const ADAPTERS: &[RuntimeDef] = &[
    CLAUDE,
    CODEX,
    OPENCODE,
    KIRO,
    COMMAND_CODE,
    ANTIGRAVITY,
    GROK,
];

/// Returns the adapter with `id`.
pub fn find(id: &str) -> Option<&'static RuntimeDef> {
    ADAPTERS.iter().find(|def| def.id == id)
}

/// Returns the adapter with `id`, or an actionable error naming the valid ids.
pub fn require(id: &str) -> Result<&'static RuntimeDef> {
    find(id).ok_or_else(|| {
        ArgoError::Invalid(format!(
            "unknown agent '{id}'; available: {}",
            ids().join(", ")
        ))
    })
}

/// All adapter ids.
pub fn ids() -> Vec<&'static str> {
    ADAPTERS.iter().map(|def| def.id).collect()
}

/// Verifies registry invariants. Called once at daemon startup.
pub fn validate() -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for def in ADAPTERS {
        if seen.contains(&def.id) {
            return Err(ArgoError::Invalid(format!(
                "duplicate adapter id: {}",
                def.id
            )));
        }
        if def.id.is_empty() || def.bin.is_empty() {
            return Err(ArgoError::Invalid(format!(
                "adapter {} must declare an id and a binary",
                def.name
            )));
        }
        if def.version_args.is_empty() {
            return Err(ArgoError::Invalid(format!(
                "adapter {} must declare version arguments for detection",
                def.id
            )));
        }
        seen.push(def.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::runtime::{PromptDelivery, StreamFormat};

    #[test]
    fn registry_invariants_hold() {
        validate().expect("registry must be valid");
    }

    #[test]
    fn the_four_mvp_adapters_are_registered() {
        assert_eq!(
            ids(),
            vec![
                "claude",
                "codex",
                "opencode",
                "kiro",
                "cmd",
                "antigravity",
                "grok"
            ]
        );
    }

    #[test]
    fn lookup_by_id_works_and_unknown_ids_are_actionable() {
        assert_eq!(find("codex").expect("codex").name, "Codex CLI");
        assert!(find("nope").is_none());
        let err = require("nope").expect_err("must fail");
        // The message must tell the user what they can pick instead.
        assert!(err.to_string().contains("claude"));
        assert!(err.to_string().contains("grok"));
    }

    #[test]
    fn every_adapter_declares_a_usable_detection_probe() {
        for def in ADAPTERS {
            assert!(
                !def.version_args.is_empty(),
                "{} needs version args",
                def.id
            );
            assert!(!def.candidate_bins().is_empty());
            assert!(!def.install_url.is_empty(), "{} needs install url", def.id);
            assert!(
                !def.fallback_models.is_empty(),
                "{} needs fallback models so the picker is never empty",
                def.id
            );
        }
    }

    #[test]
    fn transport_coverage_matches_the_mvp_plan() {
        let claude = find("claude").expect("claude");
        let codex = find("codex").expect("codex");
        let kiro = find("kiro").expect("kiro");
        let grok = find("grok").expect("grok");

        assert_eq!(
            claude.capabilities.stream_format,
            StreamFormat::ClaudeStreamJson
        );
        assert_eq!(
            codex.capabilities.stream_format,
            StreamFormat::JsonEventStream
        );
        assert_eq!(kiro.capabilities.stream_format, StreamFormat::AcpJsonRpc);
        assert_eq!(grok.capabilities.stream_format, StreamFormat::Plain);

        // Only Grok needs a staged prompt file.
        assert_eq!(grok.capabilities.prompt_delivery, PromptDelivery::File);
        assert_eq!(kiro.capabilities.prompt_delivery, PromptDelivery::Protocol);
    }

    #[test]
    fn adapters_without_resumable_sessions_are_declared() {
        let non_resumable: Vec<&str> = ADAPTERS
            .iter()
            .filter(|d| !d.capabilities.native_resume)
            .map(|d| d.id)
            .collect();
        // Command Code captures its sidecar session id; Grok has no session.
        assert_eq!(non_resumable, vec!["grok"]);
    }

    #[test]
    fn delegation_hosts_distinguish_mcp_from_the_universal_command_fallback() {
        let mcp_hosts: Vec<&str> = ADAPTERS
            .iter()
            .filter(|d| d.capabilities.delegates_via_mcp())
            .map(|d| d.id)
            .collect();
        assert_eq!(mcp_hosts, vec!["claude", "codex", "kiro"]);
        assert!(ADAPTERS
            .iter()
            .all(|definition| definition.capabilities.can_delegate()));
    }
}
