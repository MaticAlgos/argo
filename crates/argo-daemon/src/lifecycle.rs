//! Client-side daemon lifecycle helpers shared by the CLI and TUI.
//!
//! A binary upgrade can leave the previous daemon alive on the same socket. The
//! normal connection then succeeds at the transport layer but fails its protocol
//! handshake, so merely trying to spawn another daemon cannot recover: the old
//! process still owns the instance lock. These helpers recognize that state and
//! ask the older daemon to shut down using its own advertised protocol version.

use crate::protocol::{Request, Response};
use argo_core::error::{ArgoError, Result};
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const LIFECYCLE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Extracts the daemon's protocol from a rejected handshake.
///
/// Protocol v1 reported this as `INVALID_REQUEST`, so message parsing is kept as
/// a compatibility bridge. New daemons additionally use the stable
/// `PROTOCOL_MISMATCH` code.
pub fn mismatched_daemon_protocol(error: &ArgoError) -> Option<u32> {
    let (code, message) = match error {
        ArgoError::Remote { code, message, .. } => (Some(code.as_str()), message.as_str()),
        ArgoError::Invalid(message) => (None, message.as_str()),
        _ => return None,
    };
    if code != Some("PROTOCOL_MISMATCH") && !message.contains("does not match daemon v") {
        return None;
    }

    let marker = "does not match daemon v";
    let tail = message.split_once(marker)?.1;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Gracefully stops an older, protocol-incompatible daemon.
///
/// Only older daemons are stopped. If a newer daemon rejects this client, the
/// caller must update the client rather than replacing a potentially
/// schema-newer process with an old binary.
pub async fn stop_older_daemon(
    paths: &ArgoPaths,
    daemon_protocol: u32,
    client_name: &str,
) -> Result<()> {
    if daemon_protocol >= IPC_PROTOCOL_VERSION {
        return Err(ArgoError::Protocol(format!(
            "daemon protocol v{daemon_protocol} is newer than this client's v{IPC_PROTOCOL_VERSION}; update the argo client"
        )));
    }

    let stream = tokio::time::timeout(LIFECYCLE_IO_TIMEOUT, UnixStream::connect(paths.socket()))
        .await
        .map_err(|_| ArgoError::Timeout(LIFECYCLE_IO_TIMEOUT.as_millis() as u64))?
        .map_err(|e| ArgoError::Io(format!("connect to incompatible daemon: {e}")))?;
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    write_request(
        &mut writer,
        &Request::Hello {
            protocol: daemon_protocol,
            client: format!("{client_name}-upgrade/{}", env!("CARGO_PKG_VERSION")),
        },
    )
    .await?;

    let welcome = read_response(&mut reader).await?;
    match welcome {
        Response::Welcome { protocol, .. } if protocol == daemon_protocol => {}
        Response::Error { message, .. } => {
            return Err(ArgoError::Protocol(format!(
                "older daemon refused upgrade shutdown: {message}"
            )))
        }
        other => {
            return Err(ArgoError::Protocol(format!(
                "unexpected upgrade handshake reply: {other:?}"
            )))
        }
    }

    write_request(&mut writer, &Request::Shutdown).await?;
    // Reading the acknowledgement is best-effort: the daemon may close its
    // listener as soon as it broadcasts shutdown.
    let _ = read_response(&mut reader).await;
    drop(writer);

    let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(paths.socket()).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    Err(ArgoError::Timeout(SHUTDOWN_TIMEOUT.as_millis() as u64))
}

async fn write_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &Request,
) -> Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    tokio::time::timeout(LIFECYCLE_IO_TIMEOUT, writer.write_all(line.as_bytes()))
        .await
        .map_err(|_| ArgoError::Timeout(LIFECYCLE_IO_TIMEOUT.as_millis() as u64))?
        .map_err(|e| ArgoError::Io(format!("write daemon lifecycle request: {e}")))
}

async fn read_response(
    reader: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<Response> {
    let line = tokio::time::timeout(LIFECYCLE_IO_TIMEOUT, reader.next_line())
        .await
        .map_err(|_| ArgoError::Timeout(LIFECYCLE_IO_TIMEOUT.as_millis() as u64))?
        .map_err(|e| ArgoError::Io(format!("read daemon lifecycle reply: {e}")))?
        .ok_or_else(|| ArgoError::Protocol("daemon closed during lifecycle request".into()))?;
    serde_json::from_str(&line)
        .map_err(|e| ArgoError::Protocol(format!("malformed daemon lifecycle reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn parses_legacy_protocol_mismatch_messages() {
        let error = ArgoError::remote(
            "INVALID_REQUEST",
            "invalid request: client protocol v2 does not match daemon v1; restart both",
            false,
        );
        assert_eq!(mismatched_daemon_protocol(&error), Some(1));
    }

    #[test]
    fn ignores_unrelated_remote_errors() {
        let error = ArgoError::remote("INVALID_REQUEST", "invalid request: bad model", false);
        assert_eq!(mismatched_daemon_protocol(&error), None);
    }

    #[tokio::test]
    async fn older_daemon_is_handshaken_before_shutdown() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = ArgoPaths::with_root(directory.path());
        let listener = UnixListener::bind(paths.socket()).expect("bind mock daemon");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept lifecycle client");
            let (read_half, mut writer) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();

            let hello = lines.next_line().await.expect("read hello").expect("hello");
            assert!(matches!(
                Request::decode(&hello),
                Ok(Request::Hello { protocol: 1, .. })
            ));
            writer
                .write_all(
                    Response::Welcome {
                        protocol: 1,
                        version: "old".into(),
                        database: "/tmp/old.sqlite".into(),
                    }
                    .encode()
                    .as_bytes(),
                )
                .await
                .expect("welcome");

            let shutdown = lines
                .next_line()
                .await
                .expect("read shutdown")
                .expect("shutdown");
            assert!(matches!(Request::decode(&shutdown), Ok(Request::Shutdown)));
            writer
                .write_all(Response::Ok.encode().as_bytes())
                .await
                .expect("acknowledge shutdown");
        });

        stop_older_daemon(&paths, 1, "test-client")
            .await
            .expect("stop older daemon");
        server.await.expect("mock daemon task");
    }
}
