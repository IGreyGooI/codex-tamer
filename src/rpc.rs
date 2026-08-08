use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio_tungstenite::client_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use crate::config::Endpoint;

#[cfg(unix)]
const HANDSHAKE_URL: &str = "ws://localhost/rpc";
pub(crate) const CLIENT_NAME: &str = "codex-tamer";
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WEBSOCKET_MESSAGE_SIZE: usize = 128 << 20;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct RpcRequestError {
    pub method: String,
    pub error: RpcError,
}

impl fmt::Display for RpcRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", format_rpc_error(&self.method, &self.error))
    }
}

impl std::error::Error for RpcRequestError {}

#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

pub struct RpcClient {
    stream: RpcStream,
    next_id: i64,
    connection_id: u64,
    server_info: Option<InitializeInfo>,
    #[cfg_attr(not(unix), allow(dead_code))]
    peer_identity: Option<PeerIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: Option<u32>,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeInfo {
    pub user_agent: String,
    pub codex_home: std::path::PathBuf,
    pub platform_family: String,
    pub platform_os: String,
}

impl RpcClient {
    pub async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let (stream, peer_identity) = match endpoint {
            Endpoint::Unix { path } => {
                #[cfg(unix)]
                {
                    let (stream, peer_identity) = connect_unix(path).await?;
                    (RpcStream::Unix(stream), Some(peer_identity))
                }
                #[cfg(not(unix))]
                {
                    return Err(anyhow!(
                        "unix socket endpoint `{}` is unsupported on this platform; use ws:// or wss://",
                        path.display()
                    ));
                }
            }
            Endpoint::WebSocket { url, auth_token } => (
                RpcStream::Tcp(connect_websocket(url, auth_token.as_deref()).await?),
                None,
            ),
        };
        let connection_id = crate::debuglog::next_connection_id();
        if crate::debuglog::enabled() {
            let endpoint = match endpoint {
                Endpoint::Unix { path } => format!("unix://{}", path.display()),
                Endpoint::WebSocket { url, .. } => url.clone(),
            };
            crate::debuglog::log(
                "connect",
                Some(connection_id),
                json!({"endpoint": endpoint}),
            );
        }
        let mut client = Self {
            stream,
            next_id: 1,
            connection_id,
            server_info: None,
            peer_identity,
        };
        client.server_info = Some(client.initialize().await?);
        Ok(client)
    }

    pub fn server_info(&self) -> &InitializeInfo {
        self.server_info
            .as_ref()
            .expect("RpcClient is initialized before use")
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn peer_identity(&self) -> Option<PeerIdentity> {
        self.peer_identity
    }

    async fn send_message(
        &mut self,
        message: Message,
    ) -> std::result::Result<(), tokio_tungstenite::tungstenite::Error> {
        if crate::debuglog::enabled()
            && let Message::Text(text) = &message
        {
            crate::debuglog::log(
                "send",
                Some(self.connection_id),
                crate::debuglog::frame_payload(text),
            );
        }
        self.stream.send(message).await
    }

    async fn next_message(
        &mut self,
    ) -> Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> {
        let next = self.stream.next().await;
        if crate::debuglog::enabled()
            && let Some(Ok(Message::Text(text))) = &next
        {
            crate::debuglog::log(
                "recv",
                Some(self.connection_id),
                crate::debuglog::frame_payload(text),
            );
        }
        next
    }
}

enum RpcStream {
    #[cfg(unix)]
    Unix(WebSocketStream<UnixStream>),
    Tcp(WebSocketStream<MaybeTlsStream<TcpStream>>),
}

impl RpcStream {
    async fn send(
        &mut self,
        message: Message,
    ) -> std::result::Result<(), tokio_tungstenite::tungstenite::Error> {
        match self {
            #[cfg(unix)]
            RpcStream::Unix(stream) => stream.send(message).await,
            RpcStream::Tcp(stream) => stream.send(message).await,
        }
    }

    async fn next(
        &mut self,
    ) -> Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> {
        match self {
            #[cfg(unix)]
            RpcStream::Unix(stream) => stream.next().await,
            RpcStream::Tcp(stream) => stream.next().await,
        }
    }
}

#[cfg(unix)]
async fn connect_unix(
    path: &std::path::Path,
) -> Result<(WebSocketStream<UnixStream>, PeerIdentity)> {
    let request = HANDSHAKE_URL
        .into_client_request()
        .context("invalid UDS websocket handshake URL")?;
    let unix = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
        .await
        .context("timed out connecting to app-server UDS")?
        .with_context(|| format!("failed to connect to app-server UDS `{}`", path.display()))?;
    let credentials = unix
        .peer_cred()
        .with_context(|| format!("failed to inspect app-server UDS peer `{}`", path.display()))?;
    let peer_identity = PeerIdentity {
        pid: credentials
            .pid()
            .map(u32::try_from)
            .transpose()
            .context("app-server UDS peer pid is invalid")?,
        uid: credentials.uid(),
        gid: credentials.gid(),
    };
    let (stream, _) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client_async_with_config(request, unix, Some(websocket_config())),
    )
    .await
    .context("timed out upgrading UDS connection to websocket")?
    .context("failed to upgrade UDS connection to websocket")?;
    Ok((stream, peer_identity))
}

async fn connect_websocket(
    url: &str,
    auth_token: Option<&str>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    // Codex TCP app-server listeners accept WebSocket upgrades at the listener
    // root; UDS uses HANDSHAKE_URL only because tungstenite needs an HTTP URL
    // while the actual peer is the already-connected Unix stream.
    let mut request = url
        .into_client_request()
        .with_context(|| format!("invalid websocket endpoint `{url}`"))?;
    if let Some(auth_token) = auth_token {
        let header_value = HeaderValue::from_str(&format!("Bearer {auth_token}"))
            .context("invalid websocket authorization header")?;
        request.headers_mut().insert(AUTHORIZATION, header_value);
    }
    let (stream, _) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(
            request,
            Some(websocket_config()),
            /*disable_nagle*/ false,
        ),
    )
    .await
    .with_context(|| format!("timed out connecting to app-server websocket `{url}`"))?
    .with_context(|| format!("failed to connect to app-server websocket `{url}`"))?;
    Ok(stream)
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(MAX_WEBSOCKET_MESSAGE_SIZE))
        .max_message_size(Some(MAX_WEBSOCKET_MESSAGE_SIZE))
}

impl RpcClient {
    async fn initialize(&mut self) -> Result<InitializeInfo> {
        let result = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": CLIENT_NAME,
                        "title": CLIENT_NAME,
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }),
                |_| {},
            )
            .await?;
        let info: InitializeInfo =
            serde_json::from_value(result).context("initialize response has an invalid result")?;
        if info.user_agent.trim().is_empty()
            || info.platform_family.trim().is_empty()
            || info.platform_os.trim().is_empty()
            || !info.codex_home.is_absolute()
        {
            return Err(anyhow!(
                "initialize response has invalid server identity fields"
            ));
        }
        self.send_notification("initialized", Value::Null).await?;
        Ok(info)
    }

    pub async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let mut message = json!({ "method": method });
        if !params.is_null() {
            message["params"] = params;
        }
        self.send_message(Message::Text(message.to_string().into()))
            .await
            .context("failed to send notification")?;
        Ok(())
    }

    pub async fn request<F>(
        &mut self,
        method: &str,
        params: Value,
        mut on_notification: F,
    ) -> Result<Value>
    where
        F: FnMut(Notification),
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = if params.is_null() {
            json!({ "id": id, "method": method })
        } else {
            json!({ "id": id, "method": method, "params": params })
        };
        self.send_message(Message::Text(request.to_string().into()))
            .await
            .with_context(|| format!("failed to send `{method}` request"))?;

        loop {
            let next = tokio::time::timeout(REQUEST_READ_TIMEOUT, self.next_message())
                .await
                .with_context(|| format!("timed out waiting for app-server `{method}` response"))?;
            let Some(message) = next else {
                return Err(anyhow!(
                    "app-server connection closed while waiting for `{method}`"
                ));
            };
            let message = message.context("failed to read websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("app-server sent invalid JSON: {text}"))?;
            if let Some(method) = value.get("method").and_then(Value::as_str) {
                if value.get("id").is_some() {
                    self.reject_server_request(&value).await?;
                } else {
                    on_notification(Notification {
                        method: method.to_string(),
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
                continue;
            }
            if let Some(response) = parse_response_for_id(&value, id)? {
                match response {
                    ParsedResponse::Success(result) => return Ok(result),
                    ParsedResponse::Failure(error) => {
                        return Err(anyhow!(RpcRequestError {
                            method: method.to_string(),
                            error,
                        }));
                    }
                }
            }
        }
    }

    pub async fn next_notification_or_request(&mut self) -> Result<Notification> {
        loop {
            let Some(message) = self.next_message().await else {
                return Err(anyhow!("app-server connection closed"));
            };
            let message = message.context("failed to read websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("app-server sent invalid JSON: {text}"))?;
            if value.get("id").is_some() && value.get("method").is_some() {
                self.reject_server_request(&value).await?;
                continue;
            }
            if let Some(method) = value.get("method").and_then(Value::as_str) {
                return Ok(Notification {
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    async fn reject_server_request(&mut self, request: &Value) -> Result<()> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let response = json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("unsupported server request `{method}`")
            }
        });
        self.send_message(Message::Text(response.to_string().into()))
            .await
            .context("failed to reject unsupported server request")?;
        Ok(())
    }
}

enum ParsedResponse {
    Success(Value),
    Failure(RpcError),
}

fn parse_response_for_id(value: &Value, id: i64) -> Result<Option<ParsedResponse>> {
    if value.get("id").and_then(Value::as_i64) != Some(id) {
        return Ok(None);
    }
    match (value.get("result"), value.get("error")) {
        (Some(result), None) => Ok(Some(ParsedResponse::Success(result.clone()))),
        (None, Some(error)) => {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("app-server response error is missing integer `code`"))?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("app-server response error is missing string `message`"))?;
            Ok(Some(ParsedResponse::Failure(RpcError {
                code,
                message: message.to_string(),
            })))
        }
        _ => Err(anyhow!(
            "app-server response for request id {id} must contain exactly one of `result` or `error`"
        )),
    }
}

pub fn format_rpc_error(method: &str, error: &RpcError) -> String {
    if error.message.contains("experimentalApi") {
        format!("app-server rejected `{method}` because it requires experimentalApi capability")
    } else {
        format!(
            "app-server `{method}` error {}: {}",
            error.code, error.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn ordinary_request_read_timeout_is_two_minutes() {
        assert_eq!(REQUEST_READ_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn response_parser_requires_exactly_one_payload() {
        assert!(
            parse_response_for_id(&json!({"id": 2, "result": null}), 1)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            parse_response_for_id(&json!({"id": 1, "result": {"ok": true}}), 1).unwrap(),
            Some(ParsedResponse::Success(_))
        ));
        assert!(parse_response_for_id(&json!({"id": 1}), 1).is_err());
        assert!(
            parse_response_for_id(
                &json!({"id": 1, "result": null, "error": {"code": -1, "message": "bad"}}),
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn response_parser_validates_error_shape() {
        assert!(parse_response_for_id(&json!({"id": 1, "error": {}}), 1).is_err());
        let Some(ParsedResponse::Failure(error)) = parse_response_for_id(
            &json!({"id": 1, "error": {"code": -32600, "message": "invalid"}}),
            1,
        )
        .unwrap() else {
            panic!("expected error response");
        };
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "invalid");
    }

    #[tokio::test]
    async fn server_request_with_matching_id_does_not_complete_client_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_codex_home = std::env::current_dir().unwrap().join("mock-codex");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let initialize = websocket.next().await.unwrap().unwrap();
            let Message::Text(initialize) = initialize else {
                panic!("expected initialize request");
            };
            let initialize: Value = serde_json::from_str(&initialize).unwrap();
            assert_eq!(initialize["method"], "initialize");
            assert_eq!(
                initialize["params"]["clientInfo"],
                json!({
                    "name": CLIENT_NAME,
                    "title": CLIENT_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                })
            );
            websocket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {
                        "userAgent": "codex_cli_rs/0.146.0 (test)",
                        "codexHome": server_codex_home,
                        "platformFamily": "unix",
                        "platformOs": "linux"
                    }})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            let initialized = websocket.next().await.unwrap().unwrap();
            let Message::Text(initialized) = initialized else {
                panic!("expected initialized notification");
            };
            let initialized: Value = serde_json::from_str(&initialized).unwrap();
            assert_eq!(initialized["method"], "initialized");

            let request = websocket.next().await.unwrap().unwrap();
            let Message::Text(request) = request else {
                panic!("expected client request");
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "thread/read");
            let id = request["id"].clone();

            websocket
                .send(Message::Text(
                    json!({"id": id, "method": "approval/request", "params": {}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let rejection = websocket.next().await.unwrap().unwrap();
            let Message::Text(rejection) = rejection else {
                panic!("expected text rejection");
            };
            let rejection: Value = serde_json::from_str(&rejection).unwrap();
            assert_eq!(rejection["id"], id);
            assert_eq!(rejection["error"]["code"], -32601);

            websocket
                .send(Message::Text(
                    json!({"id": id, "result": {"completed": true}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let _ = websocket.next().await;
        });

        let endpoint = Endpoint::WebSocket {
            url: format!("ws://{address}"),
            auth_token: None,
        };
        let mut client = RpcClient::connect(&endpoint).await.unwrap();
        let response = client
            .request("thread/read", json!({"threadId": "thread_1"}), |_| {})
            .await
            .unwrap();

        assert_eq!(response, json!({"completed": true}));
        drop(client);
        server.await.unwrap();
    }
}
