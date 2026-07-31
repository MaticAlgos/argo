//! Argo's delegation MCP server.
//!
//! Injected into agents that support MCP, this is how one CLI hands work to a
//! different CLI. The agent calls `argo_delegate`; this server relays the request
//! to the Argo daemon, which runs the child in its own conversation and session,
//! then returns the child's reply as the tool result.
//!
//! It speaks MCP over stdio as newline-delimited JSON-RPC, because that is the
//! transport every supported CLI can launch.

use argo_core::error::{ArgoError, Result};
use argo_core::ids::{AgentId, ConversationId};
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use serde_json::{json, Value};

use crate::protocol::{Request, Response};

/// MCP protocol version this server implements.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Environment variable naming the conversation that owns this server.
///
/// Set when Argo generates the MCP configuration for a run, so a delegated child
/// is attached to the right parent.
pub const CONVERSATION_ENV: &str = "ARGO_PARENT_CONVERSATION";

/// Builds the tool catalogue advertised to the agent.
///
/// Descriptions matter: they are the only thing telling the model when handing
/// work to another CLI is worthwhile.
pub fn tools() -> Value {
    json!([
        {
            "name": "argo_list_agents",
            "description": "List the coding-agent CLIs Argo can delegate to on this machine, \
                            with their models and limitations. Call this before delegating if you \
                            are unsure which agent to choose.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "argo_delegate",
            "description": "Hand a self-contained task to a different coding-agent CLI and wait \
                            for its answer. The subagent runs in the same workspace with its own \
                            session and receives a summary of this conversation. Use this when \
                            another agent is better suited to the task, or for a second opinion. \
                            Returns the subagent's reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Adapter id, for example 'codex', 'claude', 'opencode'."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task, stated so it can be understood without this conversation."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model id for the subagent."
                    }
                },
                "required": ["agent", "task"],
                "additionalProperties": false
            }
        }
    ])
}

/// Handles one JSON-RPC line, returning the reply to write, if any.
///
/// Notifications produce no reply, which is why the return is optional.
pub async fn handle_line(paths: &ArgoPaths, line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
        // Malformed input gets a parse error rather than a silent drop, so the
        // agent learns its request failed.
        return Some(
            json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "parse error" }
            })
            .to_string(),
        );
    };

    let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = message.get("id").cloned();

    // A notification has no id and must never be answered.
    let id = id?;

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "argo", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(paths, message.get("params")).await,
        "ping" => Ok(json!({})),
        other => Err(ArgoError::Invalid(format!("unsupported method: {other}"))),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }).to_string(),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": error.to_string() }
        })
        .to_string(),
    })
}

/// Executes one tool call.
async fn call_tool(paths: &ArgoPaths, params: Option<&Value>) -> Result<Value> {
    let params = params.ok_or_else(|| ArgoError::Invalid("missing params".into()))?;
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ArgoError::Invalid("missing tool name".into()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "argo_list_agents" => list_agents(paths).await,
        "argo_delegate" => run_delegation(paths, &arguments).await,
        other => Err(ArgoError::Invalid(format!("unknown tool: {other}"))),
    }
}

/// Renders the tool result envelope MCP expects.
fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error
    })
}

async fn list_agents(paths: &ArgoPaths) -> Result<Value> {
    let mut client = DaemonClient::connect(paths).await?;
    match client
        .request(Request::ListAgents { refresh: false })
        .await?
    {
        Response::Agents { agents } => {
            let mut lines = Vec::new();
            for info in agents.iter().filter(|a| a.available) {
                let models: Vec<&str> = info.models.iter().map(|m| m.id.as_str()).take(8).collect();
                lines.push(format!(
                    "{}: {} · models: {}",
                    info.id,
                    info.version.clone().unwrap_or_default(),
                    models.join(", ")
                ));
            }
            if lines.is_empty() {
                lines.push("no coding CLI is currently available".to_string());
            }
            Ok(text_result(lines.join("\n"), false))
        }
        Response::Error { message, .. } => Ok(text_result(message, true)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

async fn run_delegation(paths: &ArgoPaths, arguments: &Value) -> Result<Value> {
    let agent = arguments
        .get("agent")
        .and_then(|a| a.as_str())
        .ok_or_else(|| ArgoError::Invalid("'agent' is required".into()))?;
    let task = arguments
        .get("task")
        .and_then(|t| t.as_str())
        .ok_or_else(|| ArgoError::Invalid("'task' is required".into()))?;
    let model = arguments
        .get("model")
        .and_then(|m| m.as_str())
        .map(|m| m.to_string());

    // Without a parent conversation the child would have nowhere to attach, and its
    // transcript would be unreachable from the TUI.
    let parent = std::env::var(CONVERSATION_ENV).map_err(|_| {
        ArgoError::Invalid(format!(
            "{CONVERSATION_ENV} is not set, so this delegation has no parent conversation"
        ))
    })?;

    let mut client = DaemonClient::connect(paths).await?;
    match client
        .request(Request::Delegate {
            parent_conversation_id: ConversationId::new(parent),
            agent_id: AgentId::new(agent),
            model,
            task: task.to_string(),
            timeout_ms: None,
        })
        .await?
    {
        Response::DelegateResult {
            agent_id,
            ok,
            output,
            conversation_id,
            ..
        } => Ok(text_result(
            format!("[subagent {agent_id} · conversation {conversation_id}]\n\n{output}"),
            !ok,
        )),
        Response::Error { message, .. } => Ok(text_result(message, true)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// A short-lived daemon connection used by one tool call.
struct DaemonClient {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
}

impl DaemonClient {
    async fn connect(paths: &ArgoPaths) -> Result<Self> {
        use tokio::io::AsyncBufReadExt;
        let stream = tokio::net::UnixStream::connect(paths.socket())
            .await
            .map_err(|e| ArgoError::Io(format!("connect to the argo daemon: {e}")))?;
        let (read_half, writer) = stream.into_split();
        let mut client = Self {
            writer,
            reader: tokio::io::BufReader::new(read_half).lines(),
        };
        match client
            .request(Request::Hello {
                protocol: IPC_PROTOCOL_VERSION,
                client: format!("argo-mcp/{}", env!("CARGO_PKG_VERSION")),
            })
            .await?
        {
            Response::Welcome { .. } => Ok(client),
            Response::Error { message, .. } => Err(ArgoError::Invalid(message)),
            other => Err(ArgoError::Protocol(format!("bad handshake: {other:?}"))),
        }
    }

    async fn request(&mut self, request: Request) -> Result<Response> {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(&request)?;
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| ArgoError::Io(format!("send: {e}")))?;
        let reply = self
            .reader
            .next_line()
            .await
            .map_err(|e| ArgoError::Io(format!("read: {e}")))?
            .ok_or_else(|| ArgoError::Protocol("the daemon closed the connection".into()))?;
        serde_json::from_str(&reply)
            .map_err(|e| ArgoError::Protocol(format!("malformed reply: {e}")))
    }
}

/// Runs the MCP server over stdio until stdin closes.
pub async fn serve_stdio(paths: &ArgoPaths) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| ArgoError::Io(format!("read stdin: {e}")))?
    {
        if let Some(reply) = handle_line(paths, &line).await {
            stdout
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .map_err(|e| ArgoError::Io(format!("write stdout: {e}")))?;
            stdout
                .flush()
                .await
                .map_err(|e| ArgoError::Io(format!("flush stdout: {e}")))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ArgoPaths {
        ArgoPaths::with_root(std::env::temp_dir().join("argo-mcp-tests"))
    }

    async fn reply(line: &str) -> Value {
        let raw = handle_line(&paths(), line)
            .await
            .expect("a request must be answered");
        serde_json::from_str(&raw).expect("valid json")
    }

    #[tokio::test]
    async fn initialize_advertises_tools() {
        let value = reply(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        assert_eq!(value["id"], json!(1));
        assert_eq!(
            value["result"]["protocolVersion"],
            json!(MCP_PROTOCOL_VERSION)
        );
        assert!(value["result"]["capabilities"]["tools"].is_object());
        assert_eq!(value["result"]["serverInfo"]["name"], json!("argo"));
    }

    #[tokio::test]
    async fn the_tool_catalogue_describes_delegation() {
        let value = reply(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
        let tools = value["result"]["tools"].as_array().expect("tools");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default())
            .collect();
        assert!(names.contains(&"argo_delegate"));
        assert!(names.contains(&"argo_list_agents"));

        let delegate = tools
            .iter()
            .find(|t| t["name"] == json!("argo_delegate"))
            .expect("delegate tool");
        // The schema is what stops the model inventing arguments.
        assert_eq!(
            delegate["inputSchema"]["required"],
            json!(["agent", "task"])
        );
        let description = delegate["description"].as_str().unwrap_or_default();
        assert!(description.contains("own session"));
    }

    #[tokio::test]
    async fn notifications_are_not_answered() {
        // Replying to a notification is a protocol violation.
        assert!(
            handle_line(&paths(), r#"{"jsonrpc":"2.0","method":"initialized"}"#)
                .await
                .is_none()
        );
        assert!(handle_line(&paths(), "   ").await.is_none());
    }

    #[tokio::test]
    async fn malformed_input_gets_a_parse_error() {
        let value = reply("{not json").await;
        assert_eq!(value["error"]["code"], json!(-32700));
    }

    #[tokio::test]
    async fn unsupported_methods_are_reported() {
        let value = reply(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#).await;
        assert!(value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unsupported method"));
    }

    #[tokio::test]
    async fn an_unknown_tool_is_reported() {
        let value = reply(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        )
        .await;
        assert!(value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown tool"));
    }

    #[tokio::test]
    async fn delegation_requires_agent_and_task() {
        let value = reply(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"argo_delegate","arguments":{"agent":"codex"}}}"#,
        )
        .await;
        assert!(value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("'task' is required"));
    }

    #[tokio::test]
    async fn delegation_without_a_parent_conversation_is_refused() {
        // Otherwise the child's transcript would be unreachable from the TUI.
        std::env::remove_var(CONVERSATION_ENV);
        let value = reply(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"argo_delegate","arguments":{"agent":"codex","task":"review"}}}"#,
        )
        .await;
        assert!(value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(CONVERSATION_ENV));
    }

    #[test]
    fn tool_results_use_the_mcp_content_envelope() {
        let ok = text_result("done", false);
        assert_eq!(ok["content"][0]["type"], json!("text"));
        assert_eq!(ok["content"][0]["text"], json!("done"));
        assert_eq!(ok["isError"], json!(false));
        assert_eq!(text_result("boom", true)["isError"], json!(true));
    }
}
