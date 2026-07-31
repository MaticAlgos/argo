//! Canonical MCP registry.
//!
//! An MCP server is configured once in Argo and translated into whatever shape
//! each CLI expects: a generated JSON config for Claude, a config override for
//! Codex, inline descriptors for ACP agents. Adapters without an injection path
//! simply do not receive them.
//!
//! Secrets are referenced by environment-variable name, never stored inline, so
//! the registry file can be read and shared without leaking credentials.

use argo_core::error::{ArgoError, Result};
use argo_core::runtime::McpInjection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How an MCP server is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// A local process speaking MCP over stdio.
    Local {
        /// Command and arguments.
        command: Vec<String>,
        /// Environment variables to set.
        #[serde(default)]
        environment: Vec<(String, String)>,
    },
    /// A remote HTTP endpoint.
    Remote {
        /// Endpoint URL.
        url: String,
        /// Headers to send. Values may reference `{env:NAME}`.
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
}

/// One registered MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Unique name; becomes the tool prefix the agent sees.
    pub name: String,
    /// Transport details.
    pub transport: McpTransport,
    /// Whether to expose it to runs.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// The persisted registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegistry {
    /// Registered servers.
    #[serde(default)]
    pub servers: Vec<McpServer>,
}

impl McpRegistry {
    /// Loads the registry, treating a missing file as empty.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) if content.trim().is_empty() => Ok(Self::default()),
            Ok(content) => serde_json::from_str(&content)
                .map_err(|e| ArgoError::Invalid(format!("mcp registry is malformed: {e}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ArgoError::Io(format!("read mcp registry: {error}"))),
        }
    }

    /// Writes the registry atomically.
    ///
    /// A crash mid-write would otherwise leave a truncated file that fails to
    /// parse, losing every configured server.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&temporary, path)
            .map_err(|e| ArgoError::Io(format!("replace mcp registry: {e}")))?;
        Ok(())
    }

    /// Adds or replaces a server.
    pub fn upsert(&mut self, server: McpServer) -> Result<()> {
        if server.name.trim().is_empty() {
            return Err(ArgoError::Invalid("mcp server name is empty".into()));
        }
        if let McpTransport::Local { command, .. } = &server.transport {
            if command.is_empty() {
                return Err(ArgoError::Invalid(format!(
                    "mcp server '{}' has no command",
                    server.name
                )));
            }
        }
        match self.servers.iter_mut().find(|s| s.name == server.name) {
            Some(existing) => *existing = server,
            None => self.servers.push(server),
        }
        Ok(())
    }

    /// Removes a server by name.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name);
        self.servers.len() != before
    }

    /// Servers that should be exposed to a run.
    pub fn active(&self) -> Vec<&McpServer> {
        self.servers.iter().filter(|s| s.enabled).collect()
    }
}

/// Config files other agents keep their MCP servers in, relative to `$HOME`.
///
/// Importing from these means a server already working in Claude or OpenCode does
/// not have to be re-entered by hand — and Argo then exposes it to every agent,
/// including ones that never had it configured.
const IMPORT_SOURCES: &[&str] = &[
    ".claude.json",
    ".claude/settings.json",
    ".config/opencode/opencode.json",
    ".config/opencode/opencode.jsonc",
    ".codex/config.json",
];

/// One server discovered in another agent's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedServer {
    /// The server, translated into Argo's canonical shape.
    pub server: McpServer,
    /// Where it was found, for display.
    pub source: String,
}

/// Strips `//` line comments so a `.jsonc` file can be parsed as JSON.
///
/// Deliberately simple: it skips anything inside a string literal, which is enough
/// for a config file and avoids pulling in a JSON5 parser.
fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Translates one JSON server entry into Argo's shape.
fn server_from_json(name: &str, value: &serde_json::Value) -> Option<McpServer> {
    // Remote form: an http/sse endpoint.
    if let Some(url) = value.get("url").and_then(|v| v.as_str()) {
        let headers = value
            .get("headers")
            .and_then(|h| h.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(key, val)| val.as_str().map(|v| (key.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        return Some(McpServer {
            name: name.to_string(),
            transport: McpTransport::Remote {
                url: url.to_string(),
                headers,
            },
            enabled: value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        });
    }

    // Local form: a command, expressed either as a string plus args (Claude) or as
    // a single array (OpenCode).
    let mut command: Vec<String> = Vec::new();
    match value.get("command") {
        Some(serde_json::Value::String(binary)) => command.push(binary.clone()),
        Some(serde_json::Value::Array(parts)) => {
            command.extend(parts.iter().filter_map(|p| p.as_str().map(String::from)));
        }
        _ => return None,
    }
    if let Some(args) = value.get("args").and_then(|a| a.as_array()) {
        command.extend(args.iter().filter_map(|a| a.as_str().map(String::from)));
    }
    if command.is_empty() {
        return None;
    }

    let environment = value
        .get("env")
        .or_else(|| value.get("environment"))
        .and_then(|e| e.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(key, val)| val.as_str().map(|v| (key.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();

    Some(McpServer {
        name: name.to_string(),
        transport: McpTransport::Local {
            command,
            environment,
        },
        enabled: value
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    })
}

/// Finds MCP servers configured in other agents' config files.
pub fn discover_importable(home: &Path) -> Vec<ImportedServer> {
    let mut found: Vec<ImportedServer> = Vec::new();

    for relative in IMPORT_SOURCES {
        let path = home.join(relative);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&strip_json_comments(&raw))
        else {
            continue;
        };
        // Vendors disagree on the key; both spellings are common.
        let servers = value
            .get("mcpServers")
            .or_else(|| value.get("mcp"))
            .and_then(|m| m.as_object());
        let Some(servers) = servers else { continue };

        for (name, entry) in servers {
            if let Some(server) = server_from_json(name, entry) {
                // First source wins, so a workspace-local definition is not
                // overwritten by a global one later in the list.
                if !found.iter().any(|i| i.server.name == server.name) {
                    found.push(ImportedServer {
                        server,
                        source: relative.to_string(),
                    });
                }
            }
        }
    }
    found
}

/// Expands `{env:NAME}` references against the process environment.
///
/// An unset variable yields an empty string rather than the literal placeholder,
/// so a missing credential fails at the server rather than sending a bogus header.
fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("{env:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Returns a copy of `server` carrying an `Authorization` header for `token`.
///
/// Argo owning the login is what lets one `argo mcp login` serve every agent,
/// including CLIs that have no way to authenticate a server themselves. An
/// explicit header configured by the user always wins, since they may be
/// deliberately overriding it.
pub fn with_bearer_token(server: &McpServer, token: &str) -> McpServer {
    let mut updated = server.clone();
    if let McpTransport::Remote { headers, .. } = &mut updated.transport {
        let already_set = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
        if !already_set {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }
    updated
}

/// Renders one server as an ACP `mcpServers` descriptor.
///
/// ACP's `mcpServers` is a *tagged* union, so the `type` discriminator is not
/// optional: without it an HTTP server is parsed as a stdio one, the missing
/// `command` fails deserialization, and the agent rejects `session/new` outright.
fn acp_descriptor(server: &McpServer) -> serde_json::Value {
    match &server.transport {
        McpTransport::Local {
            command,
            environment,
        } => serde_json::json!({
            "type": "stdio",
            "name": server.name,
            "command": command.first().cloned().unwrap_or_default(),
            "args": command.iter().skip(1).cloned().collect::<Vec<_>>(),
            "env": environment
                .iter()
                .map(|(k, v)| serde_json::json!({ "name": k, "value": expand_env(v) }))
                .collect::<Vec<_>>(),
        }),
        McpTransport::Remote { url, headers } => serde_json::json!({
            "type": "http",
            "name": server.name,
            "url": url,
            "headers": headers
                .iter()
                .map(|(k, v)| serde_json::json!({ "name": k, "value": expand_env(v) }))
                .collect::<Vec<_>>(),
        }),
    }
}

/// Renders the registry as Claude's `mcp-config` JSON.
fn claude_config(servers: &[&McpServer]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for server in servers {
        let entry = match &server.transport {
            McpTransport::Local {
                command,
                environment,
            } => {
                let mut env = serde_json::Map::new();
                for (key, value) in environment {
                    env.insert(key.clone(), serde_json::json!(expand_env(value)));
                }
                serde_json::json!({
                    "command": command.first().cloned().unwrap_or_default(),
                    "args": command.iter().skip(1).cloned().collect::<Vec<_>>(),
                    "env": env,
                })
            }
            McpTransport::Remote { url, headers } => {
                let mut header_map = serde_json::Map::new();
                for (key, value) in headers {
                    header_map.insert(key.clone(), serde_json::json!(expand_env(value)));
                }
                serde_json::json!({
                    "type": "http",
                    "url": url,
                    "headers": header_map,
                })
            }
        };
        map.insert(server.name.clone(), entry);
    }
    serde_json::json!({ "mcpServers": map })
}

/// What an adapter needs in order to reach Argo's MCP servers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpInjectionPlan {
    /// Path to a generated config file, when the adapter takes one.
    pub config_path: Option<PathBuf>,
    /// Inline descriptors, for protocol adapters.
    pub descriptors: Vec<serde_json::Value>,
    /// Names exposed, for the prompt's working-context section.
    pub names: Vec<String>,
}

/// Builds the injection plan for one adapter.
///
/// Writing the config into `staging_dir` with owner-only permissions matters:
/// expanded credentials land in that file.
pub fn plan_injection(
    registry: &McpRegistry,
    injection: McpInjection,
    staging_dir: &Path,
    run_id: &str,
) -> Result<McpInjectionPlan> {
    let active = registry.active();
    if active.is_empty() || !injection.is_supported() {
        return Ok(McpInjectionPlan::default());
    }

    let names: Vec<String> = active.iter().map(|s| s.name.clone()).collect();

    match injection {
        McpInjection::AcpSessionNew => Ok(McpInjectionPlan {
            config_path: None,
            descriptors: active.iter().map(|s| acp_descriptor(s)).collect(),
            names,
        }),
        McpInjection::ClaudeMcpJson | McpInjection::CodexConfig => {
            std::fs::create_dir_all(staging_dir)?;
            let path = staging_dir.join(format!("mcp-{run_id}.json"));
            write_private(
                &path,
                &serde_json::to_string_pretty(&claude_config(&active))?,
            )?;
            Ok(McpInjectionPlan {
                config_path: Some(path),
                descriptors: vec![],
                names,
            })
        }
        McpInjection::OpenCodeSharedConfig => {
            let path = home_dir().join(".config/opencode/opencode.jsonc");
            merge_shared_config(&path, "mcp", &active, opencode_entry, true)?;
            Ok(McpInjectionPlan {
                config_path: None,
                descriptors: vec![],
                names,
            })
        }
        McpInjection::CommandCodeSharedConfig => {
            let path = home_dir().join(".commandcode/mcp.json");
            merge_shared_config(&path, "mcpServers", &active, command_code_entry, false)?;
            Ok(McpInjectionPlan {
                config_path: None,
                descriptors: vec![],
                names,
            })
        }
        McpInjection::GeminiSharedConfig => {
            // The CLI offers no per-run config flag, so the only route is its shared
            // file. Merge into it and leave everything Argo did not add alone.
            let path = gemini_config_path();
            merge_gemini_config(&path, &active)?;
            Ok(McpInjectionPlan {
                // Reported for transparency, but not passed as an argument: the CLI
                // reads this path itself.
                config_path: None,
                descriptors: vec![],
                names,
            })
        }
        McpInjection::None => Ok(McpInjectionPlan::default()),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn opencode_entry(server: &McpServer) -> serde_json::Value {
    match &server.transport {
        McpTransport::Remote { url, headers } => serde_json::json!({
            "type": "remote",
            "url": url,
            "headers": headers.iter().map(|(k,v)| (k.clone(), expand_env(v)))
                .collect::<std::collections::BTreeMap<_,_>>()
        }),
        McpTransport::Local {
            command,
            environment,
        } => serde_json::json!({
            "type": "local",
            "command": command,
            "environment": environment.iter().map(|(k,v)| (k.clone(), expand_env(v)))
                .collect::<std::collections::BTreeMap<_,_>>()
        }),
    }
}

fn command_code_entry(server: &McpServer) -> serde_json::Value {
    match &server.transport {
        McpTransport::Remote { url, headers } => serde_json::json!({
            "transport": "http", "enabled": true, "url": url,
            "headers": headers.iter().map(|(k,v)| (k.clone(), expand_env(v)))
                .collect::<std::collections::BTreeMap<_,_>>()
        }),
        McpTransport::Local {
            command,
            environment,
        } => serde_json::json!({
            "transport": "stdio", "enabled": true,
            "command": command.first().cloned().unwrap_or_default(),
            "args": command.iter().skip(1).collect::<Vec<_>>(),
            "env": environment.iter().map(|(k,v)| (k.clone(), expand_env(v)))
                .collect::<std::collections::BTreeMap<_,_>>()
        }),
    }
}

fn parse_jsonc(raw: &str) -> Result<serde_json::Value> {
    let uncommented = strip_json_comments(raw);
    let mut cleaned = String::with_capacity(uncommented.len());
    let mut chars = uncommented.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ',' {
            let mut look = chars.clone();
            while matches!(look.peek(), Some(c) if c.is_whitespace()) {
                look.next();
            }
            if matches!(look.peek(), Some('}') | Some(']')) {
                continue;
            }
        }
        cleaned.push(ch);
    }
    serde_json::from_str(&cleaned)
        .map_err(|e| ArgoError::Invalid(format!("invalid JSON/JSONC config: {e}")))
}

fn merge_shared_config(
    path: &Path,
    root_key: &str,
    servers: &[&McpServer],
    entry: fn(&McpServer) -> serde_json::Value,
    jsonc: bool,
) -> Result<()> {
    let mut document = match std::fs::read_to_string(path) {
        Ok(raw) => {
            if jsonc {
                parse_jsonc(&raw)?
            } else {
                serde_json::from_str(&raw).map_err(|e| {
                    ArgoError::Invalid(format!(
                        "refusing to replace corrupt MCP config {}: {e}",
                        path.display()
                    ))
                })?
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(ArgoError::Io(format!("read {}: {error}", path.display()))),
    };
    let object = document.as_object_mut().ok_or_else(|| {
        ArgoError::Invalid(format!(
            "refusing to replace non-object MCP config {}",
            path.display()
        ))
    })?;
    let map = object
        .entry(root_key)
        .or_insert_with(|| serde_json::json!({}));
    let map = map.as_object_mut().ok_or_else(|| {
        ArgoError::Invalid(format!(
            "'{root_key}' in {} is not an object",
            path.display()
        ))
    })?;
    for server in servers {
        map.insert(server.name.clone(), entry(server));
    }
    write_private(path, &serde_json::to_string_pretty(&document)?)
}

/// Location of Antigravity's shared MCP configuration.
fn gemini_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini/config/mcp_config.json")
}

/// Renders one server in Antigravity's schema.
///
/// It names the endpoint `serverUrl` rather than `url`, so the shape used for the
/// other adapters is not interchangeable here.
fn gemini_entry(server: &McpServer) -> serde_json::Value {
    match &server.transport {
        McpTransport::Remote { url, headers } => {
            let mut entry = serde_json::Map::new();
            entry.insert("serverUrl".into(), serde_json::json!(url));
            if !headers.is_empty() {
                let mut map = serde_json::Map::new();
                for (key, value) in headers {
                    map.insert(key.clone(), serde_json::json!(expand_env(value)));
                }
                entry.insert("headers".into(), serde_json::Value::Object(map));
            }
            serde_json::Value::Object(entry)
        }
        McpTransport::Local {
            command,
            environment,
        } => {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "command".into(),
                serde_json::json!(command.first().cloned().unwrap_or_default()),
            );
            entry.insert(
                "args".into(),
                serde_json::json!(command.iter().skip(1).cloned().collect::<Vec<_>>()),
            );
            if !environment.is_empty() {
                let mut env = serde_json::Map::new();
                for (key, value) in environment {
                    env.insert(key.clone(), serde_json::json!(expand_env(value)));
                }
                entry.insert("env".into(), serde_json::Value::Object(env));
            }
            serde_json::Value::Object(entry)
        }
    }
}

/// Merges Argo's servers into Antigravity's shared config file.
///
/// Servers the user configured themselves are preserved: Argo's job is to add
/// servers to every agent, not to take existing ones away. Argo's own entries are
/// replaced, so a refreshed token reaches the CLI.
pub fn merge_gemini_config(path: &Path, servers: &[&McpServer]) -> Result<()> {
    let mut document = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(|error| {
            ArgoError::Invalid(format!(
                "refusing to replace corrupt Antigravity MCP config {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(ArgoError::Io(format!(
                "read Antigravity MCP config {}: {error}",
                path.display()
            )))
        }
    };
    if !document.is_object() {
        return Err(ArgoError::Invalid(format!(
            "refusing to replace non-object Antigravity MCP config {}",
            path.display()
        )));
    }

    let map = document
        .as_object_mut()
        .expect("object")
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !map.is_object() {
        *map = serde_json::json!({});
    }
    let map = map.as_object_mut().expect("object");
    for server in servers {
        map.insert(server.name.clone(), gemini_entry(server));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Owner-only: expanded auth headers may be present.
    write_private(path, &serde_json::to_string_pretty(&document)?)
}

/// Writes owner-only content atomically, since expanded secrets may be present.
fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp-config");
    let temporary = path.with_file_name(format!(".{file_name}.argo-{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|e| ArgoError::Io(format!("create {}: {e}", temporary.display())))?;
        file.write_all(content.as_bytes())
            .map_err(|e| ArgoError::Io(format!("write {}: {e}", temporary.display())))?;
        file.sync_all()
            .map_err(|e| ArgoError::Io(format!("sync {}: {e}", temporary.display())))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| ArgoError::Io(format!("secure {}: {e}", temporary.display())))?;
        }

        std::fs::rename(&temporary, path).map_err(|e| {
            ArgoError::Io(format!(
                "replace {} with private config: {e}",
                path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Explicitly chmod after rename too: OpenOptionsExt::mode applies only
            // when creating and cannot fix a pre-existing 0644 file.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| ArgoError::Io(format!("secure {}: {e}", path.display())))?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            transport: McpTransport::Local {
                command: vec!["npx".into(), "-y".into(), "server-everything".into()],
                environment: vec![("TOKEN".into(), "{env:DEMO_TOKEN}".into())],
            },
            enabled: true,
        }
    }

    /// Builds a remote server whose credential comes from `env_var`.
    ///
    /// Each test uses its own variable name: environment variables are
    /// process-global, so sharing one makes parallel tests race.
    fn remote_with_env(name: &str, env_var: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            transport: McpTransport::Remote {
                url: "https://mcp.example.com/mcp".into(),
                headers: vec![("Authorization".into(), format!("Bearer {{env:{env_var}}}"))],
            },
            enabled: true,
        }
    }

    fn remote(name: &str) -> McpServer {
        remote_with_env(name, "ARGO_TEST_UNSET_TOKEN")
    }

    #[test]
    fn json_comments_are_stripped_without_touching_strings() {
        // OpenCode ships .jsonc, and a URL containing "//" must survive.
        let input = "{\n  // a comment\n  \"url\": \"https://x.dev/mcp\" // trailing\n}";
        let stripped = strip_json_comments(input);
        assert!(!stripped.contains("a comment"));
        assert!(stripped.contains("https://x.dev/mcp"));
        let value: serde_json::Value = serde_json::from_str(&stripped).expect("valid json");
        assert_eq!(value["url"], serde_json::json!("https://x.dev/mcp"));
    }

    #[test]
    fn a_remote_server_is_imported_with_its_headers() {
        let entry = serde_json::json!({
            "type": "http",
            "url": "https://mcp.volrix.ai/mcp",
            "headers": { "Authorization": "Bearer {env:VOLRIX_TOKEN}" }
        });
        let server = server_from_json("volrix", &entry).expect("import");
        assert_eq!(server.name, "volrix");
        match server.transport {
            McpTransport::Remote { url, headers } => {
                assert_eq!(url, "https://mcp.volrix.ai/mcp");
                assert_eq!(headers.len(), 1);
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn a_local_server_is_imported_from_either_command_shape() {
        // Claude uses command + args; OpenCode uses a single array.
        let claude_shape = serde_json::json!({
            "command": "npx",
            "args": ["-y", "server-everything"],
            "env": { "TOKEN": "abc" }
        });
        let imported = server_from_json("everything", &claude_shape).expect("import");
        match imported.transport {
            McpTransport::Local {
                command,
                environment,
            } => {
                assert_eq!(command, vec!["npx", "-y", "server-everything"]);
                assert_eq!(environment, vec![("TOKEN".to_string(), "abc".to_string())]);
            }
            other => panic!("unexpected transport: {other:?}"),
        }

        let opencode_shape = serde_json::json!({ "command": ["bun", "x", "my-mcp"] });
        let imported = server_from_json("mine", &opencode_shape).expect("import");
        match imported.transport {
            McpTransport::Local { command, .. } => {
                assert_eq!(command, vec!["bun", "x", "my-mcp"]);
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn an_entry_without_a_command_or_url_is_skipped() {
        assert!(server_from_json("bad", &serde_json::json!({ "note": "nothing" })).is_none());
        assert!(server_from_json("bad", &serde_json::json!({ "command": [] })).is_none());
    }

    #[test]
    fn importable_servers_are_found_across_vendor_configs() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"volrix":{"type":"http","url":"https://mcp.volrix.ai/mcp"}}}"#,
        )
        .expect("write claude config");
        std::fs::create_dir_all(home.path().join(".config/opencode")).expect("mkdir");
        std::fs::write(
            home.path().join(".config/opencode/opencode.jsonc"),
            "{\n // comment\n \"mcp\": { \"local-one\": { \"command\": [\"bun\",\"x\",\"tool\"] } }\n}",
        )
        .expect("write opencode config");

        let found = discover_importable(home.path());
        let names: Vec<&str> = found.iter().map(|i| i.server.name.as_str()).collect();
        assert!(names.contains(&"volrix"));
        assert!(names.contains(&"local-one"));
    }

    #[test]
    fn duplicate_names_keep_the_first_source() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"dup":{"url":"https://first.example"}}}"#,
        )
        .expect("write");
        std::fs::create_dir_all(home.path().join(".config/opencode")).expect("mkdir");
        std::fs::write(
            home.path().join(".config/opencode/opencode.json"),
            r#"{"mcp":{"dup":{"url":"https://second.example"}}}"#,
        )
        .expect("write");

        let found = discover_importable(home.path());
        let dup: Vec<&ImportedServer> = found.iter().filter(|i| i.server.name == "dup").collect();
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].source, ".claude.json");
    }

    #[test]
    fn a_missing_home_yields_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover_importable(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn a_stored_token_is_attached_to_a_remote_server() {
        // This is what makes a single Argo login work on every agent, including
        // ones that cannot authenticate the server themselves.
        let server = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![],
            },
            enabled: true,
        };
        let authorized = with_bearer_token(&server, "abc123");
        match authorized.transport {
            McpTransport::Remote { headers, .. } => {
                assert_eq!(
                    headers,
                    vec![("Authorization".to_string(), "Bearer abc123".to_string())]
                );
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn an_explicit_authorization_header_is_not_overwritten() {
        // The user may be deliberately overriding what Argo stored.
        let server = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![("authorization".into(), "Bearer mine".into())],
            },
            enabled: true,
        };
        let authorized = with_bearer_token(&server, "other");
        match authorized.transport {
            McpTransport::Remote { headers, .. } => {
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0].1, "Bearer mine");
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn a_local_server_is_unaffected_by_a_token() {
        let server = McpServer {
            name: "local".into(),
            transport: McpTransport::Local {
                command: vec!["tool".into()],
                environment: vec![],
            },
            enabled: true,
        };
        assert_eq!(with_bearer_token(&server, "t"), server);
    }

    #[test]
    fn antigravity_entries_use_its_own_schema() {
        // It reads `serverUrl`, not `url`; the other adapters' shape does not apply.
        let server = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer t".into())],
            },
            enabled: true,
        };
        let entry = gemini_entry(&server);
        assert_eq!(
            entry["serverUrl"],
            serde_json::json!("https://mcp.volrix.ai/mcp")
        );
        assert_eq!(
            entry["headers"]["Authorization"],
            serde_json::json!("Bearer t")
        );
        assert!(entry.get("url").is_none());
    }

    #[test]
    fn merging_preserves_servers_argo_did_not_add() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config/mcp_config.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"theirs":{"serverUrl":"https://theirs.example"}}}"#,
        )
        .expect("seed");

        let server = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![],
            },
            enabled: true,
        };
        merge_gemini_config(&path, &[&server]).expect("merge");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "a config that may carry OAuth headers must be owner-only"
            );
        }

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        // The user's own server survives; Argo's is added alongside.
        assert_eq!(
            value["mcpServers"]["theirs"]["serverUrl"],
            serde_json::json!("https://theirs.example")
        );
        assert_eq!(
            value["mcpServers"]["volrix"]["serverUrl"],
            serde_json::json!("https://mcp.volrix.ai/mcp")
        );
    }

    #[test]
    fn merging_replaces_argos_own_entry_so_a_new_token_lands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_config.json");
        let stale = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer old".into())],
            },
            enabled: true,
        };
        merge_gemini_config(&path, &[&stale]).expect("first");
        let fresh = with_bearer_token(
            &McpServer {
                name: "volrix".into(),
                transport: McpTransport::Remote {
                    url: "https://mcp.volrix.ai/mcp".into(),
                    headers: vec![],
                },
                enabled: true,
            },
            "new",
        );
        merge_gemini_config(&path, &[&fresh]).expect("second");

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(
            value["mcpServers"]["volrix"]["headers"]["Authorization"],
            serde_json::json!("Bearer new")
        );
    }

    #[test]
    fn opencode_and_command_code_receive_their_verified_shapes() {
        let server = McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer token".into())],
            },
            enabled: true,
        };
        let open = opencode_entry(&server);
        assert_eq!(open["type"], serde_json::json!("remote"));
        assert_eq!(
            open["headers"]["Authorization"],
            serde_json::json!("Bearer token")
        );
        let command = command_code_entry(&server);
        assert_eq!(command["transport"], serde_json::json!("http"));
        assert_eq!(command["enabled"], serde_json::json!(true));
        assert_eq!(
            command["headers"]["Authorization"],
            serde_json::json!("Bearer token")
        );
    }

    #[test]
    fn opencode_jsonc_accepts_comments_and_trailing_commas() {
        let value = parse_jsonc("{ // config\n \"provider\": {}, \"mcp\": {},\n}").expect("jsonc");
        assert!(value["provider"].is_object());
        assert!(value["mcp"].is_object());
    }

    #[test]
    fn a_corrupt_shared_config_is_preserved_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_config.json");
        std::fs::write(&path, "not json at all").expect("seed");
        let server = McpServer {
            name: "x".into(),
            transport: McpTransport::Remote {
                url: "https://x.dev".into(),
                headers: vec![],
            },
            enabled: true,
        };
        let error = merge_gemini_config(&path, &[&server]).expect_err("must refuse corruption");
        assert!(error.to_string().contains("refusing to replace corrupt"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "not json at all",
            "the user's original config must survive byte-for-byte"
        );
    }

    #[test]
    fn an_acp_remote_descriptor_is_tagged_as_http() {
        // Untagged, Kiro parsed a remote server as stdio, found no `command`, and
        // rejected session/new — which took the whole turn down.
        let descriptor = acp_descriptor(&McpServer {
            name: "volrix".into(),
            transport: McpTransport::Remote {
                url: "https://mcp.volrix.ai/mcp".into(),
                headers: vec![],
            },
            enabled: true,
        });
        assert_eq!(descriptor["type"], serde_json::json!("http"));
        assert_eq!(
            descriptor["url"],
            serde_json::json!("https://mcp.volrix.ai/mcp")
        );
        assert!(descriptor.get("command").is_none());
    }

    #[test]
    fn a_missing_registry_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = McpRegistry::load(&dir.path().join("absent.json")).expect("load");
        assert!(registry.servers.is_empty());
    }

    #[test]
    fn a_malformed_registry_is_reported_rather_than_ignored() {
        // Silently discarding a broken file would lose the user's configuration.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "{ not json").expect("write");
        let error = McpRegistry::load(&path).expect_err("must fail");
        assert!(error.to_string().contains("malformed"));
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("mcp.json");
        let mut registry = McpRegistry::default();
        registry.upsert(local("everything")).expect("upsert");
        registry.upsert(remote("sentry")).expect("upsert");
        registry.save(&path).expect("save");

        let loaded = McpRegistry::load(&path).expect("load");
        assert_eq!(loaded, registry);
        // The temp file must not be left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn upsert_replaces_by_name_and_validates() {
        let mut registry = McpRegistry::default();
        registry.upsert(local("dup")).expect("first");
        registry.upsert(remote("dup")).expect("second");
        assert_eq!(registry.servers.len(), 1);
        assert!(matches!(
            registry.servers[0].transport,
            McpTransport::Remote { .. }
        ));

        assert!(registry
            .upsert(McpServer {
                name: "  ".into(),
                transport: McpTransport::Local {
                    command: vec!["x".into()],
                    environment: vec![]
                },
                enabled: true,
            })
            .is_err());
        assert!(registry
            .upsert(McpServer {
                name: "empty".into(),
                transport: McpTransport::Local {
                    command: vec![],
                    environment: vec![]
                },
                enabled: true,
            })
            .is_err());
    }

    #[test]
    fn remove_reports_whether_anything_matched() {
        let mut registry = McpRegistry::default();
        registry.upsert(local("a")).expect("upsert");
        assert!(registry.remove("a"));
        assert!(!registry.remove("a"));
    }

    #[test]
    fn disabled_servers_are_not_exposed_to_runs() {
        let mut registry = McpRegistry::default();
        registry.upsert(local("on")).expect("upsert");
        let mut off = local("off");
        off.enabled = false;
        registry.upsert(off).expect("upsert");
        let active: Vec<&str> = registry.active().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(active, vec!["on"]);
    }

    #[test]
    fn env_references_are_expanded_and_missing_ones_become_empty() {
        std::env::set_var("ARGO_TEST_EXPAND_TOKEN", "secret-value");
        assert_eq!(
            expand_env("Bearer {env:ARGO_TEST_EXPAND_TOKEN}"),
            "Bearer secret-value"
        );
        assert_eq!(expand_env("Bearer {env:ARGO_TEST_NEVER_SET}"), "Bearer ");
        // A malformed reference is passed through rather than panicking.
        assert_eq!(expand_env("{env:UNCLOSED"), "{env:UNCLOSED");
        assert_eq!(expand_env("plain"), "plain");
    }

    #[test]
    fn acp_adapters_receive_inline_descriptors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = McpRegistry::default();
        registry.upsert(local("everything")).expect("upsert");

        let plan = plan_injection(&registry, McpInjection::AcpSessionNew, dir.path(), "run-1")
            .expect("plan");
        assert!(plan.config_path.is_none());
        assert_eq!(plan.descriptors.len(), 1);
        assert_eq!(plan.descriptors[0]["name"], serde_json::json!("everything"));
        assert_eq!(plan.descriptors[0]["command"], serde_json::json!("npx"));
        // The discriminator is mandatory; ACP's mcpServers is a tagged union.
        assert_eq!(plan.descriptors[0]["type"], serde_json::json!("stdio"));
        assert_eq!(plan.names, vec!["everything".to_string()]);
    }

    #[test]
    fn claude_receives_a_generated_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = McpRegistry::default();
        registry.upsert(local("everything")).expect("upsert");
        registry.upsert(remote("sentry")).expect("upsert");

        let plan = plan_injection(&registry, McpInjection::ClaudeMcpJson, dir.path(), "run-2")
            .expect("plan");
        let path = plan.config_path.expect("config path");
        let content = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&content).expect("json");
        assert_eq!(
            value["mcpServers"]["everything"]["command"],
            serde_json::json!("npx")
        );
        assert_eq!(
            value["mcpServers"]["sentry"]["type"],
            serde_json::json!("http")
        );
        assert!(plan.descriptors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn generated_config_is_owner_only_because_it_can_contain_secrets() {
        use std::os::unix::fs::PermissionsExt;
        std::env::set_var("ARGO_TEST_PERMS_TOKEN", "super-secret");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = McpRegistry::default();
        registry
            .upsert(remote_with_env("sentry", "ARGO_TEST_PERMS_TOKEN"))
            .expect("upsert");

        let plan = plan_injection(&registry, McpInjection::ClaudeMcpJson, dir.path(), "run-3")
            .expect("plan");
        let path = plan.config_path.expect("path");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        // The expanded credential really is in there, which is why 0600 matters.
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("super-secret"));
    }

    #[test]
    fn adapters_without_an_injection_path_receive_nothing() {
        // Grok has no MCP path; handing it a config would be meaningless.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = McpRegistry::default();
        registry.upsert(local("everything")).expect("upsert");
        let plan =
            plan_injection(&registry, McpInjection::None, dir.path(), "run-4").expect("plan");
        assert_eq!(plan, McpInjectionPlan::default());
    }

    #[test]
    fn an_empty_registry_produces_no_plan_for_any_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = McpRegistry::default();
        for injection in [
            McpInjection::ClaudeMcpJson,
            McpInjection::CodexConfig,
            McpInjection::AcpSessionNew,
        ] {
            let plan = plan_injection(&registry, injection, dir.path(), "run-5").expect("plan");
            assert!(plan.config_path.is_none());
            assert!(plan.descriptors.is_empty());
        }
    }
}
