//! Live account bridge for a running Codex app-server daemon (rust-v0.160.0).
//!
//! The daemon exposes a WebSocket JSON-RPC control socket at
//! `<CODEX_HOME>/app-server-control/app-server-control.sock`. Pushing managed
//! ChatGPT OAuth tokens through `account/login/start` (chatgptAuthTokens)
//! switches the daemon's account without a restart; the daemon may then ask
//! this connection for fresh tokens via `account/chatgptAuthTokens/refresh`.
//!
//! External auth lives in daemon process memory and the refresh channel is
//! bound to the pushing connection, so the resident worker keeps one
//! persistent connection: it re-pushes the default account after every
//! reconnect and answers refresh requests. `push_account` is the one-shot
//! variant used by CLI/TUI account switching.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
const METHOD_LOGIN_START: &str = "account/login/start";
const METHOD_REFRESH: &str = "account/chatgptAuthTokens/refresh";
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_BRIDGE_ERROR: i64 = -32000;

/// Result of a best-effort token push into the running Codex daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The daemon accepted the external auth tokens; it switched live.
    Pushed,
    /// No running daemon: the control socket is missing or refused a connection.
    DaemonUnavailable,
    /// Codex proxy takeover is enabled; the daemon must authenticate through
    /// the cc-switch proxy, so pushing external auth would be wrong.
    TakeoverEnabled,
    /// The daemon build rejected the experimental chatgptAuthTokens API.
    Unsupported(String),
    /// The daemon was reachable but the push failed.
    Failed(String),
}

#[derive(Debug)]
enum BridgeError {
    /// Control socket missing or connection refused.
    Unavailable,
    /// The daemon rejected the experimental API.
    Unsupported(String),
    Failed(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Unavailable => write!(f, "codex daemon control socket unavailable"),
            BridgeError::Unsupported(reason) => write!(f, "unsupported by daemon: {reason}"),
            BridgeError::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

/// Control socket path, derived as a sibling of auth.json.
pub fn control_socket_path() -> PathBuf {
    crate::codex_config::get_codex_config_dir()
        .join("app-server-control")
        .join("app-server-control.sock")
}

async fn codex_takeover_enabled() -> Result<bool, String> {
    let db = crate::database::Database::init().map_err(|e| e.to_string())?;
    Ok(db
        .get_proxy_config_for_app_or_default("codex")
        .await
        .map_err(|e| e.to_string())?
        .enabled)
}

/// One-shot push of a managed account into the running daemon.
///
/// Never fails on daemon-side conditions; those are reported as outcomes.
/// `Err` is reserved for local failures (database, managed token store).
pub async fn push_account(account_id: &str) -> Result<PushOutcome, String> {
    if codex_takeover_enabled().await? {
        return Ok(PushOutcome::TakeoverEnabled);
    }
    if !control_socket_path().exists() {
        return Ok(PushOutcome::DaemonUnavailable);
    }
    let manager = crate::services::CodexOAuthService::manager();
    let access_token = manager
        .get_valid_token_for_account(account_id)
        .await
        .map_err(|e| format!("managed account token unavailable: {e}"))?;
    match tokio::time::timeout(PUSH_TIMEOUT, push_connected(account_id, &access_token)).await {
        Ok(Ok(())) => Ok(PushOutcome::Pushed),
        Ok(Err(BridgeError::Unavailable)) => Ok(PushOutcome::DaemonUnavailable),
        Ok(Err(BridgeError::Unsupported(reason))) => Ok(PushOutcome::Unsupported(reason)),
        Ok(Err(BridgeError::Failed(reason))) => Ok(PushOutcome::Failed(reason)),
        Err(_) => Ok(PushOutcome::Failed(format!(
            "codex daemon push timed out after {}s",
            PUSH_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(unix)]
async fn push_connected(account_id: &str, access_token: &str) -> Result<(), BridgeError> {
    let stream = connect_unix(&control_socket_path()).await?;
    let mut session = BridgeSession::connect(stream).await?;
    session.login_start(account_id, access_token).await
}

#[cfg(not(unix))]
async fn push_connected(_account_id: &str, _access_token: &str) -> Result<(), BridgeError> {
    Err(BridgeError::Unavailable)
}

#[cfg(unix)]
async fn connect_unix(path: &Path) -> Result<tokio::net::UnixStream, BridgeError> {
    match tokio::net::UnixStream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(BridgeError::Unavailable)
        }
        Err(error) => Err(BridgeError::Failed(format!(
            "connect {} failed: {error}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
async fn connect_control_socket() -> Result<tokio::net::UnixStream, BridgeError> {
    connect_unix(&control_socket_path()).await
}

/// Reconnect policy for the resident worker (injectable for tests).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReconnectBackoff {
    pub(crate) initial: Duration,
    pub(crate) cap: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(2),
            cap: Duration::from_secs(30),
        }
    }
}

/// Spawn the resident bridge worker. It keeps a persistent connection to the
/// running daemon, pushes the default managed account on every (re)connect,
/// and answers token refresh requests until the daemon goes away.
#[cfg(unix)]
pub fn spawn_resident_worker() {
    spawn_resident_worker_with(ReconnectBackoff::default());
}

#[cfg(unix)]
fn spawn_resident_worker_with(backoff: ReconnectBackoff) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        resident_loop(backoff, connect_control_socket).await;
    })
}

#[cfg(not(unix))]
pub fn spawn_resident_worker() {
    log::info!("codex daemon bridge: control socket is unix-only; worker disabled");
}

async fn resident_loop<F, Fut, S>(backoff: ReconnectBackoff, mut connector: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<S, BridgeError>>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut delay = backoff.initial;
    loop {
        match connector().await {
            Ok(stream) => match BridgeSession::connect(stream).await {
                Ok(session) => {
                    delay = backoff.initial;
                    log::info!("codex daemon bridge: connected to codex app-server control socket");
                    match session.serve().await {
                        Ok(()) => {
                            log::info!("codex daemon bridge: connection closed; will reconnect")
                        }
                        Err(error) => log::warn!(
                            "codex daemon bridge: session ended: {error}; will reconnect"
                        ),
                    }
                }
                Err(error) => {
                    log::warn!("codex daemon bridge: handshake failed: {error}; will retry")
                }
            },
            // Daemon not running; retry quietly.
            Err(BridgeError::Unavailable) => {}
            Err(error) => log::warn!("codex daemon bridge: connect failed: {error}"),
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, backoff.cap);
    }
}

/// JSON-RPC WebSocket session with the daemon, generic over the byte transport
/// so tests can drive it over TCP or in-memory duplex streams.
struct BridgeSession<S: AsyncRead + AsyncWrite + Unpin> {
    ws: WebSocketStream<S>,
    pushed_account: Option<String>,
    next_request_id: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> BridgeSession<S> {
    /// WebSocket upgrade + initialize/initialized handshake.
    async fn connect(stream: S) -> Result<Self, BridgeError> {
        let (ws, _response) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .map_err(|error| BridgeError::Failed(format!("websocket upgrade failed: {error}")))?;
        let mut session = Self {
            ws,
            pushed_account: None,
            next_request_id: 0,
        };
        session.initialize().await?;
        Ok(session)
    }

    fn alloc_request_id(&mut self) -> u64 {
        self.next_request_id += 1;
        self.next_request_id
    }

    async fn initialize(&mut self) -> Result<(), BridgeError> {
        let id = self.alloc_request_id();
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "cc-switch",
                    "title": null,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            },
        }))
        .await?;
        self.await_response(id).await?;
        // Notification: deliberately no "params" field.
        self.send_json(&json!({"jsonrpc": "2.0", "method": "initialized"}))
            .await
    }

    /// `account/login/start` with chatgptAuthTokens; on success this account
    /// becomes the one refresh requests are answered for.
    async fn login_start(
        &mut self,
        account_id: &str,
        access_token: &str,
    ) -> Result<(), BridgeError> {
        let id = self.alloc_request_id();
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": METHOD_LOGIN_START,
            "params": {
                "type": "chatgptAuthTokens",
                "accessToken": access_token,
                "chatgptAccountId": account_id,
                "chatgptPlanType": null,
            },
        }))
        .await?;
        self.await_response(id).await?;
        self.pushed_account = Some(account_id.to_string());
        Ok(())
    }

    /// Push the default managed account (unless takeover is on), then serve
    /// daemon requests until the connection drops.
    async fn serve(mut self) -> Result<(), BridgeError> {
        match codex_takeover_enabled().await {
            Ok(true) => log::info!(
                "codex daemon bridge: proxy takeover enabled; skipping external auth push"
            ),
            Ok(false) => {
                let manager = crate::services::CodexOAuthService::manager();
                match manager.default_account_id().await {
                    Some(account_id) => match manager.get_valid_token_for_account(&account_id).await
                    {
                        Ok(access_token) => match self.login_start(&account_id, &access_token).await
                        {
                            Ok(()) => log::info!(
                                "codex daemon bridge: pushed managed account {account_id} to the running daemon"
                            ),
                            // A rejected experimental API will never succeed on
                            // this daemon build; keep serving instead of
                            // reconnect-looping the same refusal.
                            Err(error @ BridgeError::Unsupported(_)) => {
                                log::warn!("codex daemon bridge: account push {error}")
                            }
                            Err(error) => return Err(error),
                        },
                        Err(error) => log::warn!(
                            "codex daemon bridge: no valid token for default account {account_id}: {error}"
                        ),
                    },
                    None => log::info!(
                        "codex daemon bridge: no default managed account; serving token refresh only"
                    ),
                }
            }
            Err(error) => log::warn!(
                "codex daemon bridge: cannot determine proxy takeover state: {error}; skipping push"
            ),
        }
        loop {
            let frame = self.next_text_frame().await?;
            let Ok(message) = serde_json::from_str::<Value>(&frame) else {
                continue;
            };
            self.handle_inbound(&message).await?;
        }
    }

    /// Read frames until the response for `want_id` arrives; daemon requests
    /// received in the meantime are still answered.
    async fn await_response(&mut self, want_id: u64) -> Result<(), BridgeError> {
        loop {
            let frame = self.next_text_frame().await?;
            let Ok(message) = serde_json::from_str::<Value>(&frame) else {
                continue;
            };
            if message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(want_id)
            {
                if let Some(error) = message.get("error") {
                    return Err(classify_rpc_error(error));
                }
                return Ok(());
            }
            self.handle_inbound(&message).await?;
        }
    }

    async fn handle_inbound(&mut self, message: &Value) -> Result<(), BridgeError> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // Stray response to a request we no longer track; ignore.
            return Ok(());
        };
        let Some(id) = message.get("id").cloned() else {
            // Notification; ignore.
            return Ok(());
        };
        if method == METHOD_REFRESH {
            self.answer_refresh(id).await
        } else {
            self.send_json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": JSONRPC_METHOD_NOT_FOUND, "message": "method not found" },
            }))
            .await
        }
    }

    async fn answer_refresh(&mut self, id: Value) -> Result<(), BridgeError> {
        // The daemon may pass a previousAccountId that differs from ours;
        // answering with the currently pushed account IS the switch semantics.
        let Some(account_id) = self.pushed_account.clone() else {
            return self
                .send_json(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": JSONRPC_BRIDGE_ERROR,
                        "message": "no account was pushed on this connection",
                    },
                }))
                .await;
        };
        let manager = crate::services::CodexOAuthService::manager();
        match manager.get_valid_token_for_account(&account_id).await {
            Ok(access_token) => {
                self.send_json(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "accessToken": access_token,
                        "chatgptAccountId": account_id,
                        "chatgptPlanType": null,
                    },
                }))
                .await
            }
            Err(error) => {
                self.send_json(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": JSONRPC_BRIDGE_ERROR,
                        "message": format!("managed account token unavailable: {error}"),
                    },
                }))
                .await
            }
        }
    }

    async fn next_text_frame(&mut self) -> Result<String, BridgeError> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => return Ok(text.to_string()),
                Some(Ok(Message::Close(_))) | None => {
                    return Err(BridgeError::Failed(
                        "codex daemon closed the control connection".to_string(),
                    ))
                }
                Some(Err(error)) => return Err(classify_ws_error(error)),
                // Binary/ping/pong frames carry no JSON-RPC payload; tungstenite
                // answers pings automatically.
                Some(Ok(_)) => {}
            }
        }
    }

    async fn send_json(&mut self, message: &Value) -> Result<(), BridgeError> {
        let text = serde_json::to_string(message)
            .map_err(|error| BridgeError::Failed(format!("serialize request failed: {error}")))?;
        self.ws
            .send(Message::Text(text))
            .await
            .map_err(classify_ws_error)
    }
}

fn classify_ws_error(error: tokio_tungstenite::tungstenite::Error) -> BridgeError {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            BridgeError::Unavailable
        }
        other => BridgeError::Failed(format!("websocket error: {other}")),
    }
}

fn classify_rpc_error(error: &Value) -> BridgeError {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    if code == JSONRPC_METHOD_NOT_FOUND || message.contains("requires experimentalApi capability") {
        BridgeError::Unsupported(message.to_string())
    } else {
        BridgeError::Failed(format!("codex daemon error {code}: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};

    fn isolated_env() -> (tempfile::TempDir, crate::test_support::TestEnvGuard) {
        let temp = tempfile::tempdir().unwrap();
        let env = crate::test_support::TestEnvGuard::isolated(temp.path());
        std::fs::create_dir_all(crate::config::get_app_config_dir()).unwrap();
        (temp, env)
    }

    async fn read_json<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Value {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.as_str()).unwrap()
                }
                Some(Ok(_)) => continue,
                other => panic!("mock daemon: unexpected frame: {other:?}"),
            }
        }
    }

    async fn send_json<S: AsyncRead + AsyncWrite + Unpin>(
        ws: &mut WebSocketStream<S>,
        value: Value,
    ) {
        ws.send(Message::Text(value.to_string())).await.unwrap();
    }

    /// Assert the initialize handshake (experimentalApi capability) and the
    /// `initialized` notification without a params field.
    async fn mock_handshake<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) {
        let initialize = read_json(ws).await;
        assert_eq!(initialize["method"], json!("initialize"));
        assert_eq!(
            initialize["params"]["capabilities"]["experimentalApi"],
            json!(true)
        );
        assert_eq!(
            initialize["params"]["clientInfo"]["name"],
            json!("cc-switch")
        );
        send_json(
            ws,
            json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "serverInfo": { "name": "mock-codex", "version": "0.160.0" } },
            }),
        )
        .await;
        let initialized = read_json(ws).await;
        assert_eq!(initialized["method"], json!("initialized"));
        assert!(initialized.get("id").is_none());
        assert!(initialized.get("params").is_none());
    }

    /// Read `account/login/start`, returning (request id, access token, account id).
    async fn mock_read_login_start<S: AsyncRead + AsyncWrite + Unpin>(
        ws: &mut WebSocketStream<S>,
    ) -> (Value, String, String) {
        let request = read_json(ws).await;
        assert_eq!(request["method"], json!(METHOD_LOGIN_START));
        assert_eq!(request["params"]["type"], json!("chatgptAuthTokens"));
        (
            request["id"].clone(),
            request["params"]["accessToken"]
                .as_str()
                .unwrap()
                .to_string(),
            request["params"]["chatgptAccountId"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    async fn mock_reply_login_success<S: AsyncRead + AsyncWrite + Unpin>(
        ws: &mut WebSocketStream<S>,
        id: Value,
    ) {
        send_json(
            ws,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "type": "chatgptAuthTokens" },
            }),
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn push_account_pushes_managed_tokens_to_running_daemon() {
        let (_temp, _env) = isolated_env();
        let _manager = crate::services::CodexOAuthService::test_manager_with_account(
            "acc-1",
            "rt-1",
            None,
            Some("at-live-1"),
            None,
        )
        .await
        .unwrap();

        let socket = control_socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mock = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            mock_handshake(&mut ws).await;
            let (id, access_token, account_id) = mock_read_login_start(&mut ws).await;
            assert_eq!(access_token, "at-live-1");
            assert_eq!(account_id, "acc-1");
            mock_reply_login_success(&mut ws, id).await;
        });

        let outcome = push_account("acc-1").await.unwrap();
        assert_eq!(outcome, PushOutcome::Pushed);
        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn push_account_reports_unsupported_when_daemon_rejects_experimental_api() {
        let (_temp, _env) = isolated_env();
        let _manager = crate::services::CodexOAuthService::test_manager_with_account(
            "acc-1",
            "rt-1",
            None,
            Some("at-live-1"),
            None,
        )
        .await
        .unwrap();

        let socket = control_socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mock = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            mock_handshake(&mut ws).await;
            let (id, _, _) = mock_read_login_start(&mut ws).await;
            send_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32600,
                        "message": "account/login/start.chatgptAuthTokens requires experimentalApi capability",
                    },
                }),
            )
            .await;
        });

        let outcome = push_account("acc-1").await.unwrap();
        match outcome {
            PushOutcome::Unsupported(reason) => {
                assert!(reason.contains("requires experimentalApi capability"))
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn push_account_reports_takeover_enabled_without_dialing() {
        let (_temp, _env) = isolated_env();
        let _manager = crate::services::CodexOAuthService::test_manager_with_account(
            "acc-1",
            "rt-1",
            None,
            Some("at-live-1"),
            None,
        )
        .await
        .unwrap();
        let db = crate::database::Database::init().unwrap();
        let mut config = db
            .get_proxy_config_for_app_or_default("codex")
            .await
            .unwrap();
        config.enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        // No socket exists: any dial attempt would report DaemonUnavailable instead.
        let outcome = push_account("acc-1").await.unwrap();
        assert_eq!(outcome, PushOutcome::TakeoverEnabled);
    }

    #[tokio::test]
    async fn push_account_reports_daemon_unavailable_when_socket_is_missing() {
        let (_temp, _env) = isolated_env();
        let _manager = crate::services::CodexOAuthService::test_manager_with_account(
            "acc-1",
            "rt-1",
            None,
            Some("at-live-1"),
            None,
        )
        .await
        .unwrap();

        let outcome = push_account("acc-1").await.unwrap();
        assert_eq!(outcome, PushOutcome::DaemonUnavailable);
    }

    #[tokio::test]
    async fn refresh_request_before_any_push_gets_error_reply() {
        let (_temp, _env) = isolated_env();
        let (client, server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(server).await.unwrap();
            mock_handshake(&mut ws).await;
            let refresh_reply = read_json(&mut ws).await;
            assert_eq!(refresh_reply["id"], json!(7));
            assert_eq!(refresh_reply["error"]["code"], json!(JSONRPC_BRIDGE_ERROR));
            // Unknown server requests must not hang either.
            let unknown_reply = read_json(&mut ws).await;
            assert_eq!(unknown_reply["id"], json!(41));
            assert_eq!(
                unknown_reply["error"]["code"],
                json!(JSONRPC_METHOD_NOT_FOUND)
            );
        });

        let mut session =
            tokio::time::timeout(Duration::from_secs(5), BridgeSession::connect(client))
                .await
                .unwrap()
                .unwrap();
        session
            .handle_inbound(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": METHOD_REFRESH,
                "params": { "reason": "unauthorized", "previousAccountId": null },
            }))
            .await
            .unwrap();
        session
            .handle_inbound(&json!({"jsonrpc": "2.0", "id": 41, "method": "thread/list"}))
            .await
            .unwrap();
        // Notifications are ignored (no reply frame).
        session
            .handle_inbound(&json!({"jsonrpc": "2.0", "method": "account/updated"}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resident_worker_pushes_serves_refresh_and_reconnects() {
        let (_temp, _env) = isolated_env();
        let _manager = crate::services::CodexOAuthService::test_manager_with_account(
            "acc-1",
            "rt-1",
            None,
            Some("at-live-1"),
            None,
        )
        .await
        .unwrap();

        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_addr = first.local_addr().unwrap();
        let second_addr = second.local_addr().unwrap();

        // First daemon: full session, then drop the connection.
        let first_server = tokio::spawn(async move {
            let (stream, _) = first.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            mock_handshake(&mut ws).await;
            let (id, access_token, account_id) = mock_read_login_start(&mut ws).await;
            assert_eq!(
                (access_token.as_str(), account_id.as_str()),
                ("at-live-1", "acc-1")
            );
            mock_reply_login_success(&mut ws, id).await;

            // previousAccountId differs on purpose: the answer must carry the
            // currently pushed account (switch semantics).
            send_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "id": 100,
                    "method": METHOD_REFRESH,
                    "params": { "reason": "unauthorized", "previousAccountId": "acc-old" },
                }),
            )
            .await;
            let refresh_reply = read_json(&mut ws).await;
            assert_eq!(refresh_reply["id"], json!(100));
            assert_eq!(refresh_reply["result"]["chatgptAccountId"], json!("acc-1"));
            assert_eq!(refresh_reply["result"]["accessToken"], json!("at-live-1"));

            send_json(
                &mut ws,
                json!({"jsonrpc": "2.0", "id": 101, "method": "server/diagnostics"}),
            )
            .await;
            let unknown_reply = read_json(&mut ws).await;
            assert_eq!(
                unknown_reply["error"]["code"],
                json!(JSONRPC_METHOD_NOT_FOUND)
            );
            // Simulate daemon death: drop the connection without a close frame.
        });

        // Second daemon (after "restart"): the worker must re-push.
        let (repushed_tx, repushed_rx) = tokio::sync::oneshot::channel::<(String, String)>();
        let second_server = tokio::spawn(async move {
            let (stream, _) = second.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            mock_handshake(&mut ws).await;
            let (id, access_token, account_id) = mock_read_login_start(&mut ws).await;
            mock_reply_login_success(&mut ws, id).await;
            let _ = repushed_tx.send((access_token, account_id));
            // Keep the connection open until the test finishes.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let attempts = Arc::new(AtomicUsize::new(0));
        let connector = {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let addr = if attempt == 0 {
                    first_addr
                } else {
                    second_addr
                };
                async move {
                    TcpStream::connect(addr)
                        .await
                        .map_err(|_| BridgeError::Unavailable)
                }
            }
        };
        let backoff = ReconnectBackoff {
            initial: Duration::from_millis(10),
            cap: Duration::from_millis(50),
        };
        let worker = tokio::spawn(resident_loop(backoff, connector));

        let (access_token, account_id) = tokio::time::timeout(Duration::from_secs(10), async {
            first_server.await.unwrap();
            repushed_rx.await.unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            (access_token.as_str(), account_id.as_str()),
            ("at-live-1", "acc-1")
        );

        worker.abort();
        second_server.abort();
    }
}
