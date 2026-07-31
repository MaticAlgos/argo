//! OAuth 2.1 login for remote MCP servers.
//!
//! Some MCP servers are protected by OAuth rather than a static token. Until now
//! Argo could only pass such a server's URL to an agent and hope that agent had
//! authenticated it already — which meant a server worked on the CLI where you
//! happened to log in and silently failed everywhere else. Argo performing the
//! login itself is what makes "configure once, available to every agent" true for
//! these servers too.
//!
//! The flow is the one the MCP specification prescribes:
//!
//! 1. Call the server unauthenticated. A `401` carries
//!    `WWW-Authenticate: Bearer resource_metadata="..."`.
//! 2. Fetch that document for the authorization servers and scopes.
//! 3. Fetch the authorization server's metadata for its endpoints.
//! 4. Register dynamically, so no client id has to be provisioned by hand.
//! 5. Authorization code with PKCE, redirected to a loopback listener.
//! 6. Exchange the code, then persist the tokens for later refresh.
//!
//! HTTP is performed with `curl` rather than by linking an HTTP and TLS stack into
//! the workspace. These are a handful of control-plane calls made during login and
//! refresh, never on a per-turn path, so the process cost is irrelevant and the
//! dependency saving is real.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argo_core::{ArgoError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Scope requested when a server does not advertise one.
const DEFAULT_SCOPE: &str = "mcp";

/// Seconds before expiry at which a token is treated as stale.
///
/// A token that expires mid-turn fails the turn, so refresh early.
const REFRESH_MARGIN_SECS: u64 = 120;

/// How long to wait for the user to finish authorizing in their browser.
const LOGIN_TIMEOUT_SECS: u64 = 300;

/// What a protected MCP server says about how to authenticate to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Canonical resource identifier to request a token for.
    pub resource: String,
    /// Authorization server issuer URL.
    pub issuer: String,
    /// Where to send the user to approve access.
    pub authorization_endpoint: String,
    /// Where to exchange a code for a token.
    pub token_endpoint: String,
    /// Where to register a client, when supported.
    pub registration_endpoint: Option<String>,
    /// Scopes to request.
    pub scopes: Vec<String>,
}

/// Credentials held for one server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredToken {
    /// Bearer token presented to the server.
    pub access_token: String,
    /// Used to obtain a new access token without involving the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds at which `access_token` stops being valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Client id this token belongs to, needed to refresh it.
    pub client_id: String,
    /// Token endpoint, recorded so refresh needs no rediscovery.
    pub token_endpoint: String,
}

impl StoredToken {
    /// True when the token should be refreshed before use.
    pub fn is_stale(&self, now: u64) -> bool {
        match self.expires_at {
            Some(expiry) => now + REFRESH_MARGIN_SECS >= expiry,
            // No expiry advertised: treat as long-lived rather than refreshing
            // constantly, since a refresh attempt can itself fail.
            None => false,
        }
    }
}

/// Tokens for every server Argo has logged into.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStore {
    /// Server name to credentials.
    #[serde(default)]
    pub tokens: BTreeMap<String, StoredToken>,
}

impl TokenStore {
    /// Loads the store, treating absence as empty.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| ArgoError::Invalid(format!("corrupt token store: {e}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ArgoError::Io(format!("read token store: {error}"))),
        }
    }

    /// Writes the store with owner-only permissions.
    ///
    /// These are live credentials, so the file must never be group or world
    /// readable, and the write is atomic so a crash cannot truncate it.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| ArgoError::Invalid(format!("serialize token store: {e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArgoError::Io(format!("create token store dir: {e}")))?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, body)
            .map_err(|e| ArgoError::Io(format!("write token store: {e}")))?;
        restrict_to_owner(&temporary)?;
        std::fs::rename(&temporary, path)
            .map_err(|e| ArgoError::Io(format!("replace token store: {e}")))?;
        Ok(())
    }
}

/// Sets `0600` on a file holding credentials.
fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ArgoError::Io(format!("secure token store: {e}")))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Default location of the token store.
pub fn token_store_path(root: &Path) -> PathBuf {
    root.join("mcp-auth.json")
}

/// Current unix time in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Encodes bytes as base64url without padding, as PKCE requires.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        // Padding is omitted, so only emit characters backed by real input bytes.
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 63] as char);
        }
    }
    out
}

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// Secret kept by Argo and sent only when redeeming the code.
    pub verifier: String,
    /// Hash sent in the authorization request.
    pub challenge: String,
}

/// Generates a PKCE pair.
///
/// PKCE is what makes a public client safe here: the authorization code is useless
/// without the verifier, which never leaves this process until redemption.
pub fn generate_pkce() -> Pkce {
    // 256 bits from two v4 UUIDs; ample for a verifier and avoids another dependency.
    let mut raw = Vec::with_capacity(32);
    raw.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    raw.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64url(&raw);
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// Extracts the `resource_metadata` URL from a `WWW-Authenticate` header.
pub fn resource_metadata_url(header: &str) -> Option<String> {
    let start = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[start..];
    // Quoted values end at the closing quote; bare ones end at the next parameter,
    // which RFC 9110 separates with a comma or whitespace.
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"').unwrap_or(quoted.len())],
        None => {
            let end = rest
                .find(|c: char| c == ',' || c.is_whitespace())
                .unwrap_or(rest.len());
            &rest[..end]
        }
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Derives the well-known metadata URL for an issuer.
///
/// Used when a server returns `401` without pointing at its metadata.
pub fn well_known_for(issuer: &str) -> String {
    format!(
        "{}/.well-known/oauth-authorization-server",
        issuer.trim_end_matches('/')
    )
}

/// Parses protected-resource metadata into an issuer and scopes.
fn parse_protected_resource(body: &str) -> Option<(String, String, Vec<String>)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let resource = value.get("resource")?.as_str()?.to_string();
    let issuer = value
        .get("authorization_servers")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();
    let scopes = value
        .get("scopes_supported")
        .and_then(|s| s.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some((resource, issuer, scopes))
}

/// Parses authorization-server metadata into endpoints.
fn parse_authorization_server(body: &str) -> Option<(String, String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    Some((
        value.get("authorization_endpoint")?.as_str()?.to_string(),
        value.get("token_endpoint")?.as_str()?.to_string(),
        value
            .get("registration_endpoint")
            .and_then(|v| v.as_str())
            .map(String::from),
    ))
}

/// Percent-encodes a value for use in a query string.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One HTTP response.
struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

/// Performs an HTTP request with `curl`.
async fn http(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Result<HttpResponse> {
    // Headers and body are separated by a sentinel so a body containing header-like
    // text cannot be mistaken for one.
    const SENTINEL: &str = "@@ARGO_BODY@@";

    let mut command = tokio::process::Command::new("curl");
    command
        .arg("-sS")
        .args(["-m", "30"])
        .args(["-X", method])
        .args(["-w", &format!("\n{SENTINEL}%{{http_code}}")])
        .arg("-D")
        .arg("-")
        .arg(url);
    for (name, value) in headers {
        command.args(["-H", &format!("{name}: {value}")]);
    }
    if let Some(body) = body {
        command.args(["--data-binary", body]);
    }

    let output = command
        .output()
        .await
        .map_err(|e| ArgoError::Io(format!("run curl: {e}. Is curl installed?")))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    let (rest, status) = match text.rsplit_once(SENTINEL) {
        Some((rest, code)) => (rest.to_string(), code.trim().parse::<u16>().unwrap_or(0)),
        None => (text, 0),
    };
    // curl writes headers then the body; they are separated by a blank line.
    let (headers, body) = match rest.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => match rest.split_once("\n\n") {
            Some((h, b)) => (h.to_string(), b.to_string()),
            None => (String::new(), rest),
        },
    };
    Ok(HttpResponse {
        status,
        headers,
        body: body.trim().to_string(),
    })
}

/// Discovers how to authenticate to `server_url`.
pub async fn discover(server_url: &str) -> Result<Discovery> {
    const PROBE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"argo","version":"1"}}}"#;

    let probe = http(
        "POST",
        server_url,
        &[
            ("Content-Type", "application/json".to_string()),
            ("Accept", "application/json, text/event-stream".to_string()),
        ],
        Some(PROBE),
    )
    .await?;

    if (200..300).contains(&probe.status) {
        return Err(ArgoError::Invalid(format!(
            "{server_url} accepted an unauthenticated request; it does not need a login"
        )));
    }

    // Prefer the pointer the server gives; fall back to its own well-known path.
    let metadata_url = probe
        .headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("www-authenticate:"))
        .and_then(resource_metadata_url)
        .or_else(|| {
            let base = server_url.split_once("://").map(|(scheme, rest)| {
                let host = rest.split('/').next().unwrap_or(rest);
                format!("{scheme}://{host}")
            })?;
            Some(format!("{base}/.well-known/oauth-protected-resource"))
        })
        .ok_or_else(|| {
            ArgoError::Invalid(format!(
                "{server_url} returned {} but advertised no OAuth metadata",
                probe.status
            ))
        })?;

    let resource_doc = http("GET", &metadata_url, &[], None).await?;
    let (resource, issuer, scopes) =
        parse_protected_resource(&resource_doc.body).ok_or_else(|| {
            ArgoError::Invalid(format!(
                "could not read OAuth metadata from {metadata_url} (HTTP {})",
                resource_doc.status
            ))
        })?;

    let as_doc = http("GET", &well_known_for(&issuer), &[], None).await?;
    let (authorization_endpoint, token_endpoint, registration_endpoint) =
        parse_authorization_server(&as_doc.body).ok_or_else(|| {
            ArgoError::Invalid(format!(
                "authorization server {issuer} did not describe its endpoints"
            ))
        })?;

    Ok(Discovery {
        resource,
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        scopes: if scopes.is_empty() {
            vec![DEFAULT_SCOPE.to_string()]
        } else {
            scopes
        },
    })
}

/// Registers Argo as a client, returning the issued client id.
async fn register_client(discovery: &Discovery, redirect_uri: &str) -> Result<String> {
    let endpoint = discovery.registration_endpoint.as_ref().ok_or_else(|| {
        ArgoError::Invalid(
            "this authorization server requires a pre-registered client; Argo cannot register itself"
                .into(),
        )
    })?;

    let request = serde_json::json!({
        "client_name": "Argo",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        // A CLI cannot keep a secret, so register as a public client and rely on PKCE.
        "token_endpoint_auth_method": "none",
        "scope": discovery.scopes.join(" "),
    });

    let response = http(
        "POST",
        endpoint,
        &[("Content-Type", "application/json".to_string())],
        Some(&request.to_string()),
    )
    .await?;

    if !(200..300).contains(&response.status) {
        return Err(ArgoError::Invalid(format!(
            "client registration failed (HTTP {}): {}",
            response.status,
            crate::truncate_for_error(&response.body)
        )));
    }
    serde_json::from_str::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| v.get("client_id")?.as_str().map(String::from))
        .ok_or_else(|| ArgoError::Invalid("registration returned no client_id".into()))
}

/// Waits on a loopback port for the authorization redirect.
///
/// Binding before the browser opens guarantees the redirect cannot arrive before
/// Argo is listening.
async fn await_redirect(listener: tokio::net::TcpListener, expected_state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let accept = async {
        loop {
            let (mut socket, _) = listener
                .accept()
                .await
                .map_err(|e| ArgoError::Io(format!("accept redirect: {e}")))?;

            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let target = request
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();

            let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
            let mut code = None;
            let mut state = None;
            let mut error = None;
            for pair in query.split('&') {
                match pair.split_once('=') {
                    Some(("code", value)) => code = Some(value.to_string()),
                    Some(("state", value)) => state = Some(value.to_string()),
                    Some(("error", value)) => error = Some(value.to_string()),
                    _ => {}
                }
            }

            // Browsers fetch /favicon.ico against the same origin; ignore anything
            // that is not the redirect rather than treating it as a failure.
            if code.is_none() && error.is_none() {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }

            let outcome = if let Some(error) = &error {
                format!("Authorization failed: {error}")
            } else if state.as_deref() != Some(expected_state) {
                // A mismatched state means this redirect is not the one Argo started.
                "Authorization failed: state mismatch".to_string()
            } else {
                "Authorized. You can close this tab and return to your terminal.".to_string()
            };

            let page = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                outcome.len() + 44,
                format_args!("<html><body style=\"font-family:sans-serif\">{outcome}</body></html>")
            );
            let _ = socket.write_all(page.as_bytes()).await;
            let _ = socket.flush().await;

            if let Some(error) = error {
                return Err(ArgoError::Invalid(format!("authorization denied: {error}")));
            }
            if state.as_deref() != Some(expected_state) {
                return Err(ArgoError::Invalid(
                    "authorization state did not match; the login was not completed by Argo".into(),
                ));
            }
            return code.ok_or_else(|| ArgoError::Invalid("redirect carried no code".into()));
        }
    };

    match tokio::time::timeout(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS), accept).await {
        Ok(result) => result,
        Err(_) => Err(ArgoError::Timeout(LOGIN_TIMEOUT_SECS * 1000)),
    }
}

/// Exchanges an authorization code, or refreshes an existing grant.
async fn request_token(
    token_endpoint: &str,
    form: Vec<(&str, String)>,
) -> Result<(String, Option<String>, Option<u64>)> {
    let body = form
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");

    let response = http(
        "POST",
        token_endpoint,
        &[(
            "Content-Type",
            "application/x-www-form-urlencoded".to_string(),
        )],
        Some(&body),
    )
    .await?;

    if !(200..300).contains(&response.status) {
        return Err(ArgoError::Invalid(format!(
            "token request failed (HTTP {}): {}",
            response.status,
            crate::truncate_for_error(&response.body)
        )));
    }

    let value: serde_json::Value = serde_json::from_str(&response.body)
        .map_err(|e| ArgoError::Invalid(format!("token response was not JSON: {e}")))?;
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ArgoError::Invalid("token response carried no access_token".into()))?
        .to_string();
    let refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_at = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|seconds| now_secs() + seconds);
    Ok((access, refresh, expires_at))
}

/// Runs the full login for one server and stores the result.
///
/// `announce` receives user-facing progress, so the caller decides how to display
/// it and this module stays free of printing.
pub async fn login(
    name: &str,
    server_url: &str,
    store_path: &Path,
    announce: &mut dyn FnMut(&str),
) -> Result<()> {
    let discovery = discover(server_url).await?;
    announce(&format!("authorization server: {}", discovery.issuer));

    // Bind first so the redirect cannot race the browser.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| ArgoError::Io(format!("bind redirect listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| ArgoError::Io(format!("resolve redirect port: {e}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let client_id = register_client(&discovery, &redirect_uri).await?;
    announce(&format!("registered client: {client_id}"));

    let pkce = generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256&resource={}",
        discovery.authorization_endpoint,
        urlencode(&client_id),
        urlencode(&redirect_uri),
        urlencode(&discovery.scopes.join(" ")),
        urlencode(&state),
        urlencode(&pkce.challenge),
        urlencode(&discovery.resource),
    );

    announce("opening your browser to authorize; if it does not open, visit:");
    announce(&authorize_url);
    open_browser(&authorize_url);

    let code = await_redirect(listener, &state).await?;
    announce("authorization received; exchanging for a token");

    let (access_token, refresh_token, expires_at) = request_token(
        &discovery.token_endpoint,
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id.clone()),
            ("code_verifier", pkce.verifier),
            ("resource", discovery.resource.clone()),
        ],
    )
    .await?;

    let mut store = TokenStore::load(store_path)?;
    store.tokens.insert(
        name.to_string(),
        StoredToken {
            access_token,
            refresh_token,
            expires_at,
            client_id,
            token_endpoint: discovery.token_endpoint,
        },
    );
    store.save(store_path)?;
    Ok(())
}

/// Reads a stored token without touching the network.
///
/// Used on the per-turn path, which must not block on HTTP. The boolean reports
/// staleness so the caller can tell the user to log in again instead of silently
/// presenting a token the server will reject.
pub fn stored_access_token(name: &str, store_path: &Path) -> Option<(String, bool)> {
    let store = TokenStore::load(store_path).ok()?;
    let token = store.tokens.get(name)?;
    Some((token.access_token.clone(), token.is_stale(now_secs())))
}

/// Returns a usable access token, refreshing it first if necessary.
///
/// Returns `None` when Argo has never logged into `name`, which is not an error:
/// most servers need no login at all.
pub async fn access_token(name: &str, store_path: &Path) -> Result<Option<String>> {
    let mut store = TokenStore::load(store_path)?;
    let Some(token) = store.tokens.get(name).cloned() else {
        return Ok(None);
    };
    if !token.is_stale(now_secs()) {
        return Ok(Some(token.access_token));
    }
    let Some(refresh) = token.refresh_token.clone() else {
        // Nothing to refresh with; hand back what exists and let the server judge.
        return Ok(Some(token.access_token));
    };

    match request_token(
        &token.token_endpoint,
        vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh),
            ("client_id", token.client_id.clone()),
        ],
    )
    .await
    {
        Ok((access_token, refresh_token, expires_at)) => {
            let updated = StoredToken {
                access_token: access_token.clone(),
                // A server may or may not rotate the refresh token.
                refresh_token: refresh_token.or(token.refresh_token),
                expires_at,
                ..token
            };
            store.tokens.insert(name.to_string(), updated);
            store.save(store_path)?;
            Ok(Some(access_token))
        }
        Err(error) => {
            // Refresh failure is recoverable by logging in again, so report the stale
            // token rather than failing the turn outright.
            tracing::warn!(%error, server = name, "refreshing the MCP token failed");
            Ok(Some(token.access_token))
        }
    }
}

/// Opens `url` in the user's browser, ignoring failure.
///
/// The URL is printed too, so a headless machine is not stuck.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_the_specified_alphabet_and_omits_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // URL-safe alphabet: never '+' or '/', and never padded.
        let encoded = base64url(&[251, 255, 190]);
        assert!(!encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='));
    }

    #[test]
    fn pkce_challenge_is_the_sha256_of_the_verifier() {
        let pkce = generate_pkce();
        assert_eq!(
            pkce.challenge,
            base64url(&Sha256::digest(pkce.verifier.as_bytes()))
        );
        // Long enough to be unguessable, and within the spec's 43..128 range.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "{}",
            pkce.verifier.len()
        );
        assert_ne!(generate_pkce().verifier, pkce.verifier, "must not repeat");
    }

    #[test]
    fn the_metadata_pointer_is_read_from_the_challenge_header() {
        let header = "www-authenticate: Bearer resource_metadata=\"https://mcp.volrix.ai/.well-known/oauth-protected-resource/mcp\"";
        assert_eq!(
            resource_metadata_url(header).as_deref(),
            Some("https://mcp.volrix.ai/.well-known/oauth-protected-resource/mcp")
        );
        // Unquoted and with following parameters.
        assert_eq!(
            resource_metadata_url("Bearer resource_metadata=https://x.dev/meta, error=\"x\"")
                .as_deref(),
            Some("https://x.dev/meta")
        );
        assert!(resource_metadata_url("Bearer realm=\"x\"").is_none());
    }

    #[test]
    fn protected_resource_metadata_yields_issuer_and_scopes() {
        let body = r#"{"resource":"https://mcp.volrix.ai/mcp",
            "authorization_servers":["https://api.volrix.ai"],
            "scopes_supported":["mcp"]}"#;
        let (resource, issuer, scopes) = parse_protected_resource(body).expect("parsed");
        assert_eq!(resource, "https://mcp.volrix.ai/mcp");
        assert_eq!(issuer, "https://api.volrix.ai");
        assert_eq!(scopes, vec!["mcp"]);
        assert!(parse_protected_resource("{}").is_none());
    }

    #[test]
    fn authorization_server_metadata_yields_endpoints() {
        let body = r#"{"issuer":"https://api.volrix.ai",
            "authorization_endpoint":"https://api.volrix.ai/oauth/authorize",
            "token_endpoint":"https://api.volrix.ai/oauth/token",
            "registration_endpoint":"https://api.volrix.ai/oauth/register"}"#;
        let (authorize, token, register) = parse_authorization_server(body).expect("parsed");
        assert_eq!(authorize, "https://api.volrix.ai/oauth/authorize");
        assert_eq!(token, "https://api.volrix.ai/oauth/token");
        assert_eq!(
            register.as_deref(),
            Some("https://api.volrix.ai/oauth/register")
        );
        // Registration is optional; endpoints are not.
        assert!(parse_authorization_server(r#"{"token_endpoint":"t"}"#).is_none());
    }

    #[test]
    fn well_known_path_tolerates_a_trailing_slash() {
        assert_eq!(
            well_known_for("https://api.volrix.ai/"),
            "https://api.volrix.ai/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn urlencoding_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("s p:a/c e?"), "s%20p%3Aa%2Fc%20e%3F");
        assert_eq!(urlencode("safe-_.~"), "safe-_.~");
    }

    #[test]
    fn a_token_is_refreshed_before_it_expires_not_after() {
        let token = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(1_000),
            client_id: "c".into(),
            token_endpoint: "t".into(),
        };
        assert!(!token.is_stale(1_000 - REFRESH_MARGIN_SECS - 1));
        // Inside the margin: refresh now rather than fail mid-turn.
        assert!(token.is_stale(1_000 - REFRESH_MARGIN_SECS));
        assert!(token.is_stale(2_000));
    }

    #[test]
    fn a_token_without_an_expiry_is_not_treated_as_stale() {
        let token = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
            client_id: "c".into(),
            token_endpoint: "t".into(),
        };
        assert!(!token.is_stale(u64::MAX / 2));
    }

    #[test]
    fn the_token_store_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = token_store_path(dir.path());
        assert!(TokenStore::load(&path)
            .expect("missing is empty")
            .tokens
            .is_empty());

        let mut store = TokenStore::default();
        store.tokens.insert(
            "volrix".into(),
            StoredToken {
                access_token: "secret".into(),
                refresh_token: Some("r".into()),
                expires_at: Some(42),
                client_id: "c".into(),
                token_endpoint: "https://api.volrix.ai/oauth/token".into(),
            },
        );
        store.save(&path).expect("save");

        let loaded = TokenStore::load(&path).expect("load");
        assert_eq!(loaded.tokens["volrix"].access_token, "secret");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "credentials must not be readable by others"
            );
        }
        // The temporary file must not be left behind holding a copy.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn the_synchronous_lookup_reports_staleness_without_network_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = token_store_path(dir.path());
        assert!(stored_access_token("volrix", &path).is_none());

        let mut store = TokenStore::default();
        store.tokens.insert(
            "volrix".into(),
            StoredToken {
                access_token: "live".into(),
                refresh_token: None,
                // Already expired.
                expires_at: Some(1),
                client_id: "c".into(),
                token_endpoint: "t".into(),
            },
        );
        store.save(&path).expect("save");
        let (token, stale) = stored_access_token("volrix", &path).expect("token");
        assert_eq!(token, "live");
        assert!(stale, "an expired token must be reported as stale");
    }

    #[tokio::test]
    async fn a_server_never_logged_into_has_no_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = token_store_path(dir.path());
        assert_eq!(access_token("absent", &path).await.expect("lookup"), None);
    }
}
