//! Telegram Bot API client.
//!
//! Requests go out through `curl`, matching how the rest of Argo reaches the
//! network (see `argo-runtime`'s updater). That keeps the workspace free of an
//! HTTP client dependency for what is a handful of JSON endpoints.
//!
//! The transport is behind a trait so the bridge's behaviour — throttling,
//! retries, allowlisting, formatting — is testable without a bot token or a
//! network.

use argo_core::error::{ArgoError, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

/// Bot API base, overridable so tests never reach the real service.
const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Ceiling on one API call, independent of the long-poll window.
const CALL_OVERHEAD_SECS: u64 = 15;

/// A bounded number of retries prevents one pathological Bot API response from
/// holding the bridge forever while still honoring Telegram's requested delay.
const RETRY_AFTER_ATTEMPTS: usize = 2;

/// How Telegram should interpret message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    /// Telegram's MarkdownV2 dialect.
    MarkdownV2,
    /// No markup; every character is literal.
    Plain,
}

impl ParseMode {
    fn as_param(&self) -> Option<&'static str> {
        match self {
            Self::MarkdownV2 => Some("MarkdownV2"),
            Self::Plain => None,
        }
    }
}

/// Whether Telegram rejected message formatting rather than delivery itself.
///
/// Plain-text fallback is safe only for these errors. Retrying authorization,
/// routing, rate-limit, or server failures as plain text would duplicate traffic
/// and hide the real failure.
pub fn is_parse_entity_error(error: &ArgoError) -> bool {
    let ArgoError::Remote { code, message, .. } = error else {
        return false;
    };
    if code != "TELEGRAM_ERROR" {
        return false;
    }
    let message = message.to_ascii_lowercase();
    message.contains("can't parse entities")
        || message.contains("can't find end of the entity")
        || message.contains("can't find end of bold entity")
        || message.contains("can't find end of italic entity")
        || message.contains("character '") && message.contains("is reserved")
}

/// Carries one JSON request to the Bot API and returns the decoded reply.
///
/// Implemented by [`CurlTransport`] in production and by fakes in tests.
pub trait Transport: Send + Sync {
    /// Invokes `method` with `body`, returning the raw response envelope.
    fn call(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + '_>>;
}

/// Real transport: one `curl` invocation per call.
pub struct CurlTransport {
    token: String,
    api_base: String,
}

impl CurlTransport {
    /// Builds a transport for `token` against the public Bot API.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            api_base: std::env::var("ARGO_TELEGRAM_API_BASE")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
        }
    }
}

fn curl_config_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], "")
}

fn transport_deadline(timeout: Duration) -> Duration {
    timeout + Duration::from_secs(CALL_OVERHEAD_SECS)
}

fn curl_invocation(url: &str, body_path: &std::path::Path, seconds: u64) -> (Vec<String>, String) {
    let body = format!("@{}", body_path.display());
    let config = format!(
        "silent\nshow-error\nlocation\nmax-time = \"{seconds}\"\nheader = \"Content-Type: application/json\"\ndata-binary = \"{}\"\nurl = \"{}\"\n",
        curl_config_quote(&body),
        curl_config_quote(url),
    );
    (vec!["--config".into(), "-".into()], config)
}

fn write_private_body(path: &std::path::Path, body: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)?;
    file.sync_all()?;
    Ok(())
}

impl Transport for CurlTransport {
    fn call(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + '_>> {
        let url = format!("{}/bot{}/{}", self.api_base, self.token, method);
        let payload = body.to_string();
        let request_seconds = timeout.as_secs().max(1);
        let deadline = transport_deadline(timeout);
        let curl_seconds = deadline.as_secs().max(1);
        let method = method.to_string();
        Box::pin(async move {
            // Both secrets are absent from argv: the token-bearing URL travels in
            // curl's stdin config, and the JSON body sits briefly in a random
            // owner-only file named by a harmless argv path.
            let body_path = std::env::temp_dir().join(format!(
                ".argo-telegram-body-{}.json",
                argo_core::RunId::generate()
            ));
            write_private_body(&body_path, payload.as_bytes())?;
            let (args, config) = curl_invocation(&url, &body_path, curl_seconds);
            let run = async {
                let mut command = tokio::process::Command::new("curl");
                command
                    .args(&args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                let mut child = command.spawn()?;
                {
                    use tokio::io::AsyncWriteExt;
                    let mut stdin = child.stdin.take().expect("stdin was piped");
                    stdin.write_all(config.as_bytes()).await?;
                    stdin.shutdown().await?;
                }
                child.wait_with_output().await
            };

            let result = tokio::time::timeout(deadline, run).await;
            let _ = std::fs::remove_file(&body_path);
            let output = result
                .map_err(|_| ArgoError::Timeout(request_seconds * 1000))?
                .map_err(|error| ArgoError::Process(format!("telegram {method}: {error}")))?;

            if !output.status.success() && output.stdout.is_empty() {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(ArgoError::Process(format!(
                    "telegram {method} failed: {}",
                    if detail.is_empty() {
                        output.status.to_string()
                    } else {
                        detail
                    }
                )));
            }
            serde_json::from_slice(&output.stdout).map_err(|error| {
                ArgoError::Protocol(format!("telegram {method} returned invalid JSON: {error}"))
            })
        })
    }
}

/// Who the token belongs to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BotIdentity {
    /// Numeric bot id.
    pub id: i64,
    /// `@username`, without the leading sigil.
    pub username: String,
}

/// A message the bot received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    /// Update sequence number, used to advance the poll offset.
    pub update_id: i64,
    /// Chat the message arrived in.
    pub chat_id: i64,
    /// Message id, for replies and reactions.
    pub message_id: i64,
    /// Sender's numeric user id, checked against the allowlist.
    pub from_id: i64,
    /// Message text, already trimmed.
    pub text: String,
}

impl IncomingMessage {
    /// Telegram private chats use the human's user id as the chat id.
    pub fn is_private_chat(&self) -> bool {
        self.chat_id == self.from_id
    }
}

/// A button press on an inline keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackQuery {
    /// Update sequence number.
    pub update_id: i64,
    /// Telegram's id for this press, which must be acknowledged.
    pub id: String,
    /// Chat the keyboard lives in.
    pub chat_id: i64,
    /// Message carrying the keyboard, so it can be edited in place.
    pub message_id: i64,
    /// Presser's numeric user id, checked against the allowlist.
    pub from_id: i64,
    /// Opaque payload set when the button was built.
    pub data: String,
}

impl CallbackQuery {
    /// Telegram private chats use the human's user id as the chat id.
    pub fn is_private_chat(&self) -> bool {
        self.chat_id == self.from_id
    }
}

/// One raw `getUpdates` result after parsing actionable updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateBatch {
    /// Text messages and callbacks the bridge understands.
    pub updates: Vec<Update>,
    /// Highest id in the raw envelope, including unsupported updates.
    pub high_water: Option<i64>,
}

/// One update worth acting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// A text message.
    Message(IncomingMessage),
    /// An inline-keyboard button press.
    Callback(CallbackQuery),
}

impl Update {
    /// Sequence number, for advancing the poll offset.
    pub fn update_id(&self) -> i64 {
        match self {
            Self::Message(message) => message.update_id,
            Self::Callback(query) => query.update_id,
        }
    }

    /// Who sent it.
    pub fn from_id(&self) -> i64 {
        match self {
            Self::Message(message) => message.from_id,
            Self::Callback(query) => query.from_id,
        }
    }

    /// Whether the update came from the sender's one-to-one bot chat.
    pub fn is_private_chat(&self) -> bool {
        match self {
            Self::Message(message) => message.is_private_chat(),
            Self::Callback(query) => query.is_private_chat(),
        }
    }
}

/// Longest payload Telegram accepts on an inline button.
///
/// Hard limit, and exceeding it makes `sendMessage` fail outright — which is why
/// callback payloads address choices by index rather than by name.
pub const MAX_CALLBACK_DATA: usize = 64;

/// A row of labelled buttons.
pub type KeyboardRow = Vec<(String, String)>;

/// Builds the `reply_markup` value for an inline keyboard.
fn inline_keyboard(rows: &[KeyboardRow]) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|(label, data)| json!({ "text": label, "callback_data": data }))
                .collect::<Vec<_>>()
                .into()
        })
        .collect();
    json!({ "inline_keyboard": rows })
}

/// A typed Bot API client over any [`Transport`].
pub struct Bot {
    transport: Box<dyn Transport>,
}

impl Bot {
    /// Wraps a transport.
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }

    /// Convenience constructor for the real API.
    pub fn with_token(token: impl Into<String>) -> Self {
        Self::new(Box::new(CurlTransport::new(token)))
    }

    /// Unwraps a Bot API envelope, turning `ok: false` into an error.
    ///
    /// A `429` is surfaced as [`ArgoError::Timeout`] carrying `retry_after` so
    /// callers can back off for exactly as long as Telegram asked.
    fn unwrap_envelope(method: &str, envelope: Value) -> Result<Value> {
        if envelope.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
        }
        let description = envelope
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        if let Some(retry_after) = envelope
            .get("parameters")
            .and_then(|p| p.get("retry_after"))
            .and_then(Value::as_i64)
        {
            return Err(ArgoError::Timeout((retry_after.max(1) as u64) * 1000));
        }
        Err(ArgoError::Remote {
            code: "TELEGRAM_ERROR".into(),
            message: format!("{method}: {description}"),
            retryable: envelope
                .get("error_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code >= 500),
        })
    }

    async fn call(&self, method: &str, body: Value, timeout: Duration) -> Result<Value> {
        for attempt in 0..=RETRY_AFTER_ATTEMPTS {
            let envelope = self.transport.call(method, body.clone(), timeout).await?;
            match Self::unwrap_envelope(method, envelope) {
                Err(ArgoError::Timeout(milliseconds)) if attempt < RETRY_AFTER_ATTEMPTS => {
                    tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                }
                result => return result,
            }
        }
        unreachable!("bounded Bot API retry loop always returns")
    }

    /// Confirms the token and reports the bot's identity.
    pub async fn get_me(&self) -> Result<BotIdentity> {
        let result = self
            .call("getMe", json!({}), Duration::from_secs(10))
            .await?;
        serde_json::from_value(result)
            .map_err(|error| ArgoError::Protocol(format!("getMe returned no identity: {error}")))
    }

    /// Removes any webhook so this bot can use `getUpdates`.
    ///
    /// Pending updates are retained: callers establish and persist their own
    /// high-water mark before accepting an authorization command.
    pub async fn delete_webhook(&self) -> Result<()> {
        self.call(
            "deleteWebhook",
            json!({ "drop_pending_updates": false }),
            Duration::from_secs(10),
        )
        .await
        .map(|_| ())
    }

    /// Long-polls for new messages starting at `offset`.
    ///
    /// Only plain-text messages are returned; anything else (edits, joins, media)
    /// is skipped, but its `update_id` still advances the offset so an
    /// unsupported message cannot wedge the poll loop forever.
    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<UpdateBatch> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    "allowed_updates": ["message", "callback_query"],
                }),
                Duration::from_secs(timeout_secs),
            )
            .await?;
        Ok(UpdateBatch {
            updates: parse_updates(&result),
            high_water: highest_update_id(&result),
        })
    }

    /// Sends a message, returning its id so it can be edited as the run streams.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        mode: ParseMode,
        silent: bool,
    ) -> Result<i64> {
        self.send(chat_id, text, mode, silent, &[]).await
    }

    /// Sends a message, optionally carrying an inline keyboard.
    ///
    /// An empty `rows` is not the same as an empty keyboard: Telegram rejects
    /// `inline_keyboard: []` on some clients, so the field is omitted entirely
    /// rather than sent blank.
    pub async fn send(
        &self,
        chat_id: i64,
        text: &str,
        mode: ParseMode,
        silent: bool,
        rows: &[KeyboardRow],
    ) -> Result<i64> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "disable_notification": silent,
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(parse_mode) = mode.as_param() {
            body["parse_mode"] = json!(parse_mode);
        }
        if !rows.is_empty() {
            body["reply_markup"] = inline_keyboard(rows);
        }
        let result = self
            .call("sendMessage", body, Duration::from_secs(20))
            .await?;
        result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ArgoError::Protocol("sendMessage returned no message_id".into()))
    }

    /// Rewrites a message and its keyboard together.
    ///
    /// Used to advance a wizard in place. Passing no rows removes the keyboard,
    /// which is how a finished wizard stops accepting further presses — leaving
    /// stale buttons live would let one tap replay a step that already ran.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        mode: ParseMode,
        rows: &[KeyboardRow],
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
            "reply_markup": inline_keyboard(rows),
        });
        if let Some(parse_mode) = mode.as_param() {
            body["parse_mode"] = json!(parse_mode);
        }
        match self
            .call("editMessageText", body, Duration::from_secs(20))
            .await
        {
            Ok(_) => Ok(()),
            Err(ArgoError::Remote { message, .. })
                if message.contains("message is not modified") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Acknowledges a button press, optionally showing a short toast.
    ///
    /// Telegram keeps a spinner on the button until this call lands, so it is
    /// made before the work the press triggers — otherwise a slow action is
    /// indistinguishable from a broken one.
    pub async fn answer_callback(&self, callback_id: &str, text: Option<&str>) -> Result<()> {
        let mut body = json!({ "callback_query_id": callback_id });
        if let Some(text) = text {
            body["text"] = json!(text);
        }
        self.call("answerCallbackQuery", body, Duration::from_secs(10))
            .await
            .map(|_| ())
    }

    /// Replaces the text of a message already in the chat.
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        mode: ParseMode,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(parse_mode) = mode.as_param() {
            body["parse_mode"] = json!(parse_mode);
        }
        match self
            .call("editMessageText", body, Duration::from_secs(20))
            .await
        {
            Ok(_) => Ok(()),
            // Editing to identical text is an error upstream but a no-op here:
            // the throttle can easily fire with nothing new to show.
            Err(ArgoError::Remote { message, .. })
                if message.contains("message is not modified") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Sets (or with `None`, clears) the single reaction on a message.
    ///
    /// Reactions are decoration: a failure here must never interrupt a turn, so
    /// the result is intentionally discarded by callers.
    pub async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: Option<&str>,
    ) -> Result<()> {
        let reaction = match emoji {
            Some(emoji) => json!([{ "type": "emoji", "emoji": emoji }]),
            None => json!([]),
        };
        self.call(
            "setMessageReaction",
            json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reaction": reaction,
            }),
            Duration::from_secs(10),
        )
        .await
        .map(|_| ())
    }

    /// Publishes the command menu so the commands autocomplete in Telegram.
    pub async fn set_my_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let commands: Vec<Value> = commands
            .iter()
            .map(|(command, description)| json!({ "command": command, "description": description }))
            .collect();
        self.call(
            "setMyCommands",
            json!({ "commands": commands }),
            Duration::from_secs(10),
        )
        .await
        .map(|_| ())
    }
}

/// Extracts the text messages from a `getUpdates` result.
fn parse_updates(result: &Value) -> Vec<Update> {
    let Some(updates) = result.as_array() else {
        return Vec::new();
    };
    updates
        .iter()
        .filter_map(|update| {
            let update_id = update.get("update_id")?.as_i64()?;
            if let Some(query) = update.get("callback_query") {
                let message = query.get("message")?;
                return Some(Update::Callback(CallbackQuery {
                    update_id,
                    id: query.get("id")?.as_str()?.to_string(),
                    chat_id: message.get("chat")?.get("id")?.as_i64()?,
                    message_id: message.get("message_id")?.as_i64()?,
                    from_id: query.get("from")?.get("id")?.as_i64()?,
                    data: query.get("data")?.as_str()?.to_string(),
                }));
            }
            let message = update.get("message")?;
            Some(Update::Message(IncomingMessage {
                update_id,
                chat_id: message.get("chat")?.get("id")?.as_i64()?,
                message_id: message.get("message_id")?.as_i64()?,
                from_id: message.get("from")?.get("id")?.as_i64()?,
                text: message.get("text")?.as_str()?.trim().to_string(),
            }))
        })
        .collect()
}

/// Highest `update_id` in a batch, used to advance the poll offset.
///
/// Derived from the raw envelope rather than the parsed messages so updates the
/// bridge ignores are still acknowledged; otherwise one unsupported message
/// would be redelivered forever.
pub fn highest_update_id(result: &Value) -> Option<i64> {
    result
        .as_array()?
        .iter()
        .filter_map(|update| update.get("update_id")?.as_i64())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_envelope_yields_its_result() {
        let value = Bot::unwrap_envelope("getMe", json!({ "ok": true, "result": { "id": 7 } }))
            .expect("ok envelope");
        assert_eq!(value.get("id").and_then(Value::as_i64), Some(7));
    }

    #[test]
    fn rate_limiting_reports_the_exact_wait_telegram_asked_for() {
        // Guessing a backoff either wastes time or gets throttled again; Telegram
        // states the number, so it must survive into the error.
        let error = Bot::unwrap_envelope(
            "sendMessage",
            json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 7",
                "parameters": { "retry_after": 7 },
            }),
        )
        .expect_err("must fail");
        assert!(matches!(error, ArgoError::Timeout(7000)), "{error:?}");
    }

    #[test]
    fn client_errors_are_not_marked_retryable_but_server_errors_are() {
        let client = Bot::unwrap_envelope(
            "sendMessage",
            json!({ "ok": false, "error_code": 400, "description": "chat not found" }),
        )
        .expect_err("must fail");
        assert!(!client.is_retryable());
        assert!(client.to_string().contains("chat not found"));

        let server = Bot::unwrap_envelope(
            "sendMessage",
            json!({ "ok": false, "error_code": 502, "description": "Bad Gateway" }),
        )
        .expect_err("must fail");
        assert!(server.is_retryable());
    }

    #[test]
    fn only_text_messages_are_parsed_but_every_update_advances_the_offset() {
        // A photo or a chat-join update must not stall the poll loop: it is
        // skipped for handling yet still acknowledged.
        let result = json!([
            {
                "update_id": 10,
                "message": {
                    "message_id": 1,
                    "chat": { "id": -100 },
                    "from": { "id": 42 },
                    "text": "  hello  "
                }
            },
            { "update_id": 11, "message": { "message_id": 2, "chat": { "id": -100 }, "from": { "id": 42 } } },
            { "update_id": 12, "edited_message": { "message_id": 1 } },
        ]);

        let messages = parse_updates(&result);
        assert_eq!(messages.len(), 1);
        let Update::Message(message) = &messages[0] else {
            panic!("expected a text message");
        };
        assert_eq!(message.text, "hello");
        assert_eq!(message.from_id, 42);
        assert_eq!(message.chat_id, -100);
        assert_eq!(highest_update_id(&result), Some(12));
    }

    #[test]
    fn a_button_press_is_parsed_with_everything_needed_to_answer_it() {
        let result = json!([{
            "update_id": 20,
            "callback_query": {
                "id": "cb-1",
                "from": { "id": 42 },
                "data": "a:2",
                "message": { "message_id": 9, "chat": { "id": -100 } }
            }
        }]);
        let updates = parse_updates(&result);
        assert_eq!(updates.len(), 1);
        let Update::Callback(query) = &updates[0] else {
            panic!("expected a callback");
        };
        assert_eq!(query.id, "cb-1");
        assert_eq!(query.data, "a:2");
        // The message id is what lets the wizard advance in place instead of
        // posting a new message per step.
        assert_eq!(query.message_id, 9);
        assert_eq!(query.from_id, 42);
        assert_eq!(updates[0].update_id(), 20);
        assert_eq!(updates[0].from_id(), 42);
    }

    #[test]
    fn a_keyboard_serializes_as_rows_of_labelled_buttons() {
        let markup = inline_keyboard(&[
            vec![
                ("claude".into(), "a:0".into()),
                ("codex".into(), "a:1".into()),
            ],
            vec![("cancel".into(), "x".into())],
        ]);
        let rows = markup["inline_keyboard"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].as_array().expect("row").len(), 2);
        assert_eq!(rows[0][0]["text"], "claude");
        assert_eq!(rows[0][0]["callback_data"], "a:0");
    }

    #[test]
    fn curl_deadline_includes_connection_overhead_beyond_the_long_poll() {
        let deadline = transport_deadline(Duration::from_secs(25));
        assert_eq!(deadline, Duration::from_secs(40));
        let (_, config) = curl_invocation(
            "https://api.telegram.org/botTOKEN/getUpdates",
            std::path::Path::new("/tmp/body.json"),
            deadline.as_secs(),
        );
        assert!(config.contains("max-time = \"40\""), "{config}");
    }

    #[test]
    fn an_empty_poll_leaves_the_offset_alone() {
        assert!(parse_updates(&json!([])).is_empty());
        assert_eq!(highest_update_id(&json!([])), None);
    }

    #[test]
    fn only_sender_owned_chat_ids_are_private() {
        let private = IncomingMessage {
            update_id: 1,
            chat_id: 42,
            message_id: 2,
            from_id: 42,
            text: "/status".into(),
        };
        let group = IncomingMessage {
            chat_id: -100,
            ..private.clone()
        };
        assert!(private.is_private_chat());
        assert!(!group.is_private_chat());
        assert!(Update::Message(private).is_private_chat());
        assert!(!Update::Message(group).is_private_chat());
    }

    #[test]
    fn plain_fallback_is_limited_to_entity_parse_errors() {
        let parse = Bot::unwrap_envelope(
            "sendMessage",
            json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: can't parse entities: Character '-' is reserved"
            }),
        )
        .expect_err("parse error");
        assert!(is_parse_entity_error(&parse));

        let routing = Bot::unwrap_envelope(
            "sendMessage",
            json!({ "ok": false, "error_code": 400, "description": "chat not found" }),
        )
        .expect_err("routing error");
        assert!(!is_parse_entity_error(&routing));
        assert!(!is_parse_entity_error(&ArgoError::Timeout(1000)));
    }
}
