//! Argo shared resources.
//!
//! Skills and MCP servers are configured once in Argo and made available to every
//! adapter that can accept them. Discovery deliberately reads the directories the
//! user's existing CLIs already use, so adopting Argo does not require moving or
//! reinstalling anything.

pub mod instructions;
pub mod mcp;
pub mod oauth;
pub mod skills;
pub mod staging;

pub use instructions::Instructions;
pub use mcp::{
    discover_importable, with_bearer_token, ImportedServer, McpInjectionPlan, McpRegistry,
    McpServer, McpTransport,
};
pub use skills::{discover, is_valid_name, Skill, SkillOrigin};
pub use staging::{cleanup_legacy_workspace_cache, render_prompt_section, stage, StagedSkill};

/// Shortens a server's error body for inclusion in a message.
///
/// Remote errors can be whole HTML pages; a bounded excerpt keeps the cause
/// visible without flooding the terminal.
pub(crate) fn truncate_for_error(body: &str) -> String {
    const LIMIT: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(LIMIT).collect();
    format!("{kept}…")
}
