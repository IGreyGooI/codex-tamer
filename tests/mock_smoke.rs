#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::net::TcpListener as StdTcpListener;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use assert_cmd::Command;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;

struct MockServer {
    _temp: TempDir,
    _managed_runtime_temp: Option<TempDir>,
    socket: PathBuf,
    config: PathBuf,
    received: Arc<Mutex<Vec<Value>>>,
    managed_home: Option<PathBuf>,
    managed_runtime: Option<PathBuf>,
}

struct TcpMockServer {
    _temp: TempDir,
    endpoint: String,
    config: PathBuf,
}

type ResponseOverrides = Arc<HashMap<String, Value>>;

fn managed_runtime_fixture(_temp: &TempDir) -> (PathBuf, Option<TempDir>) {
    #[cfg(target_os = "macos")]
    {
        let runtime = tempfile::Builder::new()
            .prefix("codex-tamer-")
            .tempdir_in("/tmp")
            .expect("short managed runtime tempdir");
        let path = runtime.path().to_path_buf();
        (path, Some(runtime))
    }

    #[cfg(not(target_os = "macos"))]
    {
        (_temp.path().join("runtime"), None)
    }
}

#[derive(Clone)]
struct GoalState {
    objective: String,
    status: String,
    token_budget: i64,
}

impl TcpMockServer {
    fn start(auth_token: Option<&'static str>) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let config = temp.path().join("config.toml");
        let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock tcp socket");
        let addr = std_listener.local_addr().expect("local addr");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = format!("ws://{addr}");
        fs::write(
            &config,
            match auth_token {
                Some(token) => format!(
                    "[servers.work]\nendpoint = \"{}\"\nauth_token = \"{}\"\n",
                    endpoint, token
                ),
                None => format!("[servers.work]\nendpoint = \"{}\"\n", endpoint),
            },
        )
        .expect("config");
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_thread = Arc::clone(&received);
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(std_listener).expect("tokio listener");
                loop {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let received = Arc::clone(&received_for_thread);
                    tokio::spawn(async move {
                        let expected_auth = auth_token.map(|token| format!("Bearer {token}"));
                        #[allow(clippy::result_large_err)]
                        let websocket = accept_hdr_async(
                            stream,
                            move |request: &Request, response: Response| {
                                let actual = request
                                    .headers()
                                    .get("authorization")
                                    .and_then(|value| value.to_str().ok())
                                    .map(ToString::to_string);
                                assert_eq!(actual, expected_auth);
                                Ok(response)
                            },
                        )
                        .await
                        .expect("websocket accept");
                        handle_websocket(
                            websocket,
                            received,
                            MockBehavior::new(
                                TurnNotificationMode::Complete,
                                false,
                                RejectFirst::none(),
                                Arc::new(HashMap::new()),
                            ),
                        )
                        .await;
                    });
                }
            });
        });

        Self {
            _temp: temp,
            endpoint,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("codex-tamer").expect("binary");
        command
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .env_remove("CODEX_TAMER_STATE")
            .env_remove("XDG_STATE_HOME")
            .arg("--config")
            .arg(&self.config);
        command
    }
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            objective: "Finish".to_string(),
            status: "active".to_string(),
            token_budget: 1234,
        }
    }
}

#[derive(Clone, Copy)]
enum TurnNotificationMode {
    Complete,
    None,
    WrongTurnCompleted,
    Failed,
    UnknownStatus,
}

#[derive(Clone, Copy)]
enum RejectFirstMethod {
    None,
    TurnStart,
    TurnSteer,
    SettingsUpdate,
    TurnsList,
}

#[derive(Clone, Copy)]
struct RejectFirst {
    method: RejectFirstMethod,
    code: i64,
    message: Option<&'static str>,
    fail_usage_refresh_after_redemption: bool,
}

impl RejectFirst {
    const fn none() -> Self {
        Self {
            method: RejectFirstMethod::None,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn method(method: RejectFirstMethod) -> Self {
        Self {
            method,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn method_with_error(
        method: RejectFirstMethod,
        code: i64,
        message: &'static str,
    ) -> Self {
        Self {
            method,
            code,
            message: Some(message),
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn with_usage_refresh_failure() -> Self {
        Self {
            method: RejectFirstMethod::None,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: true,
        }
    }
}

#[derive(Clone)]
struct MockBehavior {
    turn_notification_mode: TurnNotificationMode,
    malformed_turn_start: bool,
    reject_first: RejectFirst,
    rejected_first_method: Arc<Mutex<bool>>,
    goal_state: Arc<Mutex<HashMap<String, GoalState>>>,
    response_overrides: ResponseOverrides,
}

impl MockBehavior {
    fn new(
        turn_notification_mode: TurnNotificationMode,
        malformed_turn_start: bool,
        reject_first: RejectFirst,
        response_overrides: ResponseOverrides,
    ) -> Self {
        Self {
            turn_notification_mode,
            malformed_turn_start,
            reject_first,
            rejected_first_method: Arc::new(Mutex::new(false)),
            goal_state: Arc::new(Mutex::new(HashMap::new())),
            response_overrides,
        }
    }
}

impl MockServer {
    fn start() -> Self {
        Self::start_with_options(TurnNotificationMode::Complete, false, RejectFirst::none())
    }

    fn start_with_usage_refresh_failure() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::with_usage_refresh_failure(),
        )
    }

    fn start_without_turn_notifications() -> Self {
        Self::start_with_options(TurnNotificationMode::None, false, RejectFirst::none())
    }

    fn start_with_malformed_turn_start() -> Self {
        Self::start_with_options(TurnNotificationMode::None, true, RejectFirst::none())
    }

    fn start_requiring_resume_for_send() -> Self {
        Self::start_with_options(
            TurnNotificationMode::None,
            false,
            RejectFirst::method(RejectFirstMethod::TurnStart),
        )
    }

    fn start_requiring_resume_for_steer() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::method(RejectFirstMethod::TurnSteer),
        )
    }

    fn start_requiring_resume_for_settings_set() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::method(RejectFirstMethod::SettingsUpdate),
        )
    }

    fn start_rejecting_turn_start_with(code: i64, message: &'static str) -> Self {
        Self::start_with_options(
            TurnNotificationMode::None,
            false,
            RejectFirst::method_with_error(RejectFirstMethod::TurnStart, code, message),
        )
    }

    fn start_with_unmaterialized_first_poll() -> Self {
        Self::start_rejecting_first_turns_list_with(
            -32600,
            "thread thread_1 is not materialized yet; thread/turns/list is unavailable before first user message",
        )
    }

    fn start_rejecting_first_turns_list_with(code: i64, message: &'static str) -> Self {
        Self::start_with_options(
            TurnNotificationMode::None,
            false,
            RejectFirst::method_with_error(RejectFirstMethod::TurnsList, code, message),
        )
    }

    fn start_with_wrong_turn_completion() -> Self {
        Self::start_with_options(
            TurnNotificationMode::WrongTurnCompleted,
            false,
            RejectFirst::none(),
        )
    }

    fn start_with_failed_turn() -> Self {
        Self::start_with_options(TurnNotificationMode::Failed, false, RejectFirst::none())
    }

    fn start_with_unknown_turn_status() -> Self {
        Self::start_with_options(
            TurnNotificationMode::UnknownStatus,
            false,
            RejectFirst::none(),
        )
    }

    fn start_with_options(
        turn_notification_mode: TurnNotificationMode,
        malformed_turn_start: bool,
        reject_first: RejectFirst,
    ) -> Self {
        Self::start_with_options_and_responses(
            turn_notification_mode,
            malformed_turn_start,
            reject_first,
            Arc::new(HashMap::new()),
        )
    }

    fn start_with_response(method: &str, response: Value) -> Self {
        let responses = HashMap::from([(method.to_string(), response)]);
        Self::start_with_options_and_responses(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::none(),
            Arc::new(responses),
        )
    }

    fn start_with_options_and_responses(
        turn_notification_mode: TurnNotificationMode,
        malformed_turn_start: bool,
        reject_first: RejectFirst,
        response_overrides: ResponseOverrides,
    ) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let socket = temp.path().join("codex.sock");
        let config = temp.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "[servers.work]\ntype = \"uds\"\npath = \"{}\"\n",
                socket.display()
            ),
        )
        .expect("config");
        let std_listener = StdUnixListener::bind(&socket).expect("bind mock socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let behavior = MockBehavior::new(
            turn_notification_mode,
            malformed_turn_start,
            reject_first,
            response_overrides,
        );
        let received = spawn_mock_listener(std_listener, behavior);

        Self {
            _temp: temp,
            _managed_runtime_temp: None,
            socket,
            config,
            received,
            managed_home: None,
            managed_runtime: None,
        }
    }

    fn start_managed() -> Self {
        Self::start_managed_with_user_agent("codex_cli_rs/0.146.0 (test)")
    }

    fn start_managed_with_user_agent(user_agent: &str) -> Self {
        Self::start_managed_with_initialize_override(user_agent, None)
    }

    fn start_managed_with_initialize_override(
        user_agent: &str,
        initialize_override: Option<Value>,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let managed_home = temp.path().join("codex-home");
        let (managed_runtime, managed_runtime_temp) = managed_runtime_fixture(&temp);
        let fake_user_home = temp.path().join("user-home");
        fs::create_dir_all(&managed_home).expect("codex home");
        fs::create_dir_all(&managed_runtime).expect("runtime");
        fs::create_dir_all(&fake_user_home).expect("user home");
        fs::set_permissions(&managed_runtime, fs::Permissions::from_mode(0o700))
            .expect("runtime permissions");
        let managed_home = fs::canonicalize(managed_home).expect("canonical codex home");
        let digest = blake3::hash(managed_home.as_os_str().as_encoded_bytes());
        let socket_dir = managed_runtime
            .join("codex-tamer")
            .join(&digest.to_hex().as_str()[..24]);
        fs::create_dir_all(&socket_dir).expect("socket dir");
        fs::set_permissions(
            managed_runtime.join("codex-tamer"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("managed root permissions");
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))
            .expect("socket dir permissions");
        let socket = socket_dir.join("app-server.sock");
        let config = fake_user_home.join(".config/codex-tamer/config.toml");
        let std_listener = StdUnixListener::bind(&socket).expect("bind managed mock socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let initialize = initialize_override.unwrap_or_else(|| {
            json!({
                "userAgent": user_agent,
                "codexHome": managed_home.clone(),
                "platformFamily": "unix",
                "platformOs": "linux"
            })
        });
        let responses = Arc::new(HashMap::from([("initialize".to_string(), initialize)]));
        let behavior = MockBehavior::new(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::none(),
            responses,
        );
        let received = spawn_mock_listener(std_listener, behavior);

        Self {
            _temp: temp,
            _managed_runtime_temp: managed_runtime_temp,
            socket,
            config,
            received,
            managed_home: Some(managed_home),
            managed_runtime: Some(managed_runtime),
        }
    }

    fn endpoint(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("codex-tamer").expect("binary");
        command
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .env_remove("CODEX_TAMER_STATE")
            .env_remove("XDG_STATE_HOME")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn managed_command(&self) -> Command {
        let mut command = Command::cargo_bin("codex-tamer").expect("binary");
        let user_home = self
            .config
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
            .expect("fake user home");
        command
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .env("HOME", user_home)
            .env(
                "CODEX_HOME",
                self.managed_home.as_ref().expect("managed codex home"),
            )
            .env(
                "XDG_RUNTIME_DIR",
                self.managed_runtime.as_ref().expect("managed runtime"),
            );
        command
    }

    fn std_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("codex-tamer"));
        command
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .env_remove("CODEX_TAMER_STATE")
            .env_remove("XDG_STATE_HOME")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn allow_rate_limit_reset(&self) {
        let config = fs::read_to_string(&self.config).expect("read config");
        fs::write(
            &self.config,
            format!("{config}\nallow_rate_limit_reset = true\n"),
        )
        .expect("write config");
    }

    fn methods(&self) -> Vec<String> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter_map(|request| request["method"].as_str().map(ToString::to_string))
            .collect()
    }

    fn params_for(&self, method: &str) -> Vec<Value> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter(|request| request["method"].as_str() == Some(method))
            .map(|request| request["params"].clone())
            .collect()
    }
}

fn spawn_mock_listener(
    std_listener: StdUnixListener,
    behavior: MockBehavior,
) -> Arc<Mutex<Vec<Value>>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_for_thread = Arc::clone(&received);
    thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let listener = UnixListener::from_std(std_listener).expect("tokio listener");
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let received = Arc::clone(&received_for_thread);
                let behavior = behavior.clone();
                tokio::spawn(async move {
                    handle_connection(stream, received, behavior).await;
                });
            }
        });
    });
    received
}

#[test]
#[ignore]
fn managed_listener_process_helper() {
    let Some(socket) = std::env::var_os("CODEX_TAMER_TEST_MANAGED_SOCKET") else {
        return;
    };
    let codex_home =
        std::env::var_os("CODEX_TAMER_TEST_MANAGED_HOME").expect("managed helper Codex home");
    let std_listener = StdUnixListener::bind(PathBuf::from(socket)).expect("bind managed helper");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let behavior = MockBehavior::new(
        TurnNotificationMode::Complete,
        false,
        RejectFirst::none(),
        Arc::new(HashMap::from([(
            "initialize".to_string(),
            json!({
                "userAgent": "codex_cli_rs/0.146.0 (test)",
                "codexHome": PathBuf::from(codex_home),
                "platformFamily": "unix",
                "platformOs": "linux"
            }),
        )])),
    );
    let received = Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async move {
        let listener = UnixListener::from_std(std_listener).expect("tokio listener");
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            let received = Arc::clone(&received);
            let behavior = behavior.clone();
            tokio::spawn(async move {
                handle_connection(stream, received, behavior).await;
            });
        }
    });
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    received: Arc<Mutex<Vec<Value>>>,
    behavior: MockBehavior,
) {
    let Ok(ws) = accept_async(stream).await else {
        return;
    };
    handle_websocket(ws, received, behavior).await;
}

async fn handle_websocket<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    received: Arc<Mutex<Vec<Value>>>,
    behavior: MockBehavior,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).expect("json request");
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            received.lock().expect("received").push(value.clone());
            if let Some(id) = value.get("id").cloned() {
                if method == "thread/read" && thread_id(&value) == "thread_missing" {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32600,
                            "message": "thread not found: thread_missing"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if method == "thread/read" && thread_id(&value) == "thread_error" {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": "temporary read failure"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if should_reject_first_method(
                    method,
                    behavior.reject_first.method,
                    &behavior.rejected_first_method,
                ) {
                    let message = behavior
                        .reject_first
                        .message
                        .map(ToString::to_string)
                        .unwrap_or_else(|| format!("thread not found: {}", thread_id(&value)));
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": behavior.reject_first.code,
                            "message": message
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if behavior.reject_first.fail_usage_refresh_after_redemption
                    && method == "account/rateLimits/read"
                    && received.lock().expect("received").iter().any(|request| {
                        request["method"].as_str() == Some("account/rateLimitResetCredit/consume")
                    })
                {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": "usage refresh unavailable"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if method == "thread/turns/list" && thread_id(&value) == "thread_hung_poll" {
                    continue;
                }
                if method == "thread/resume" && thread_id(&value) == "thread_hung_resume" {
                    continue;
                }
                if method == "thread/turns/list"
                    && thread_id(&value) == "thread_unmaterialized_terminal"
                {
                    send_terminal_notification(&mut ws, "thread_unmaterialized_terminal").await;
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32600,
                            "message": "thread thread_unmaterialized_terminal is not materialized yet"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if method == "thread/resume" && thread_id(&value) == "thread_snapshot_replay" {
                    send_snapshot_replay_notifications(&mut ws, "thread_snapshot_replay").await;
                }
                let result = behavior
                    .response_overrides
                    .get(method)
                    .cloned()
                    .unwrap_or_else(|| {
                        mock_result(
                            method,
                            &value,
                            behavior.malformed_turn_start,
                            &behavior.goal_state,
                        )
                    });
                if method == "turn/start" {
                    let thread_id = value["params"]["threadId"].as_str().unwrap_or("thread_1");
                    send_turn_notifications(&mut ws, thread_id, behavior.turn_notification_mode)
                        .await;
                }
                let response = json!({ "id": id, "result": result });
                if ws
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn send_terminal_notification(
    ws: &mut tokio_tungstenite::WebSocketStream<
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    >,
    thread_id: &str,
) {
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turn": {
                        "id": "turn_1",
                        "status": "completed",
                        "items": []
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

async fn send_snapshot_replay_notifications(
    ws: &mut tokio_tungstenite::WebSocketStream<
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    >,
    thread_id: &str,
) {
    for notification in [
        json!({
            "method": "item/started",
            "params": {
                "threadId": thread_id,
                "turnId": "turn_1",
                "item": {"id": "msg_live", "type": "agentMessage"}
            }
        }),
        json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "turnId": "turn_1",
                "item": {"id": "msg_live", "type": "agentMessage", "text": "done"}
            }
        }),
    ] {
        let _ = ws
            .send(Message::Text(notification.to_string().into()))
            .await;
    }
}

fn should_reject_first_method(
    method: &str,
    reject_first_method: RejectFirstMethod,
    rejected_first_method: &Arc<Mutex<bool>>,
) -> bool {
    let expected = match reject_first_method {
        RejectFirstMethod::None => return false,
        RejectFirstMethod::TurnStart => "turn/start",
        RejectFirstMethod::TurnSteer => "turn/steer",
        RejectFirstMethod::SettingsUpdate => "thread/settings/update",
        RejectFirstMethod::TurnsList => "thread/turns/list",
    };
    if method != expected {
        return false;
    }
    let mut rejected = rejected_first_method.lock().expect("rejected first method");
    if *rejected {
        return false;
    }
    *rejected = true;
    true
}

async fn send_turn_notifications(
    ws: &mut tokio_tungstenite::WebSocketStream<
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    >,
    thread_id: &str,
    mode: TurnNotificationMode,
) {
    let (turn_id, terminal_status, text) = match mode {
        TurnNotificationMode::Complete => ("turn_1", "completed", "done"),
        TurnNotificationMode::WrongTurnCompleted => ("turn_other", "failed", "done"),
        TurnNotificationMode::Failed => ("turn_1", "failed", "failed"),
        TurnNotificationMode::UnknownStatus => ("turn_1", "mystery", "mystery"),
        TurnNotificationMode::None => return,
    };
    let terminal_error = if matches!(mode, TurnNotificationMode::Failed) {
        json!({"code": "mock_failure", "message": "mock turn failed"})
    } else {
        Value::Null
    };
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": thread_id,
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "delta": text
                }
            })
            .to_string()
            .into(),
        ))
        .await;
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "turnId": "turn_1",
                    "item": {
                        "id": "item_agent",
                        "type": "agentMessage",
                        "text": text
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turn": {
                        "id": turn_id,
                        "status": terminal_status,
                        "items": [],
                        "error": terminal_error
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

fn mock_result(
    method: &str,
    request: &Value,
    malformed_turn_start: bool,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> Value {
    match method {
        "initialize" => json!({
            "userAgent": "mock-codex",
            "codexHome": "/tmp/mock-codex",
            "platformFamily": "unix",
            "platformOs": "linux"
        }),
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/paged") => {
            paged_threads(request)
        }
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/sorted") => {
            sorted_desc_threads(request)
        }
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/multiline") => {
            page(json!([sample_multiline_preview_thread("thread_multiline")]))
        }
        "thread/list" if request["params"]["parentThreadId"].is_string() => {
            page(json!([sample_thread_with_parent(
                "thread_child_1",
                request["params"]["parentThreadId"]
                    .as_str()
                    .unwrap_or("thread_parent")
            )]))
        }
        "thread/list" if request["params"]["ancestorThreadId"].is_string() => page(json!([
            sample_thread_with_parent("thread_grandchild_1", "thread_child_1"),
            sample_thread_with_parent(
                "thread_child_1",
                request["params"]["ancestorThreadId"]
                    .as_str()
                    .unwrap_or("thread_parent")
            )
        ])),
        "thread/list" if request["params"]["isPinned"].as_bool() == Some(true) => {
            page(json!([sample_pinned_thread("thread_pinned")]))
        }
        "thread/list" if request["params"]["isPinned"].as_bool() == Some(false) => {
            page(json!([sample_thread("thread_unpinned")]))
        }
        "thread/list" => page(json!([sample_thread("thread_1")])),
        "thread/search" if request["params"]["searchTerm"].as_str() == Some("paged") => {
            paged_search_results(request)
        }
        "thread/search" => page(json!([{ "thread": sample_thread("thread_1"), "score": 1.0 }])),
        "thread/read" => {
            let mut thread = sample_thread(thread_id(request));
            if thread_id(request) == "thread_read_only" {
                thread["canAcceptDirectInput"] = json!(false);
            }
            json!({ "thread": thread })
        }
        "thread/turns/list" if thread_id(request) == "thread_result_paged" => {
            paged_turn_results(request)
        }
        "thread/turns/list" if thread_id(request) == "thread_result_empty_page" => json!({
            "data": [],
            "nextCursor": "unique-empty-page"
        }),
        "thread/turns/list" if thread_id(request) == "thread_invalid_next_cursor" => json!({
            "data": [{"id": "turn_other", "status": "completed", "items": []}],
            "nextCursor": 7
        }),
        "thread/turns/list" if thread_id(request) == "thread_missing_turn_data" => json!({}),
        "thread/turns/list" if thread_id(request) == "thread_missing_status" => {
            page(json!([{"id": "turn_bad", "items": []}]))
        }
        "thread/turns/list" if thread_id(request) == "thread_invalid_status" => {
            page(json!([{"id": "turn_bad", "status": 7, "items": []}]))
        }
        "thread/turns/list" => page(json!([sample_turn()])),
        "thread/start" => json!({
            "thread": sample_thread("thread_new"),
            "model": request["params"]["model"].as_str().unwrap_or("gpt-5.1-codex"),
            "reasoningEffort": request["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("medium"),
            "serviceTier": request["params"].get("serviceTier").cloned().unwrap_or(Value::Null)
        }),
        "thread/fork" => json!({
            "thread": sample_forked_thread(
                "thread_fork",
                request["params"]["threadId"].as_str().unwrap_or("thread_1")
            ),
            "model": request["params"]["model"].as_str().unwrap_or("gpt-5.1-codex"),
            "reasoningEffort": request["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("medium"),
            "serviceTier": request["params"].get("serviceTier").cloned().unwrap_or(Value::Null)
        }),
        "thread/name/set" => json!({}),
        "thread/metadata/update" => {
            let mut thread = sample_thread(thread_id(request));
            thread["isPinned"] = request["params"]["isPinned"].clone();
            json!({ "thread": thread })
        }
        "turn/start" if malformed_turn_start => {
            json!({ "turn": { "status": "inProgress", "items": [] } })
        }
        "turn/start" => json!({ "turn": { "id": "turn_1", "status": "inProgress", "items": [] } }),
        "thread/resume" => {
            let mut thread = sample_thread(thread_id(request));
            if thread_id(request) == "thread_denied_after_resume" {
                thread["canAcceptDirectInput"] = json!(false);
            }
            let mut turn = sample_turn();
            if matches!(
                thread_id(request),
                "thread_hung_poll" | "thread_missing_turn_data"
            ) {
                turn["status"] = json!("inProgress");
                turn["completedAt"] = Value::Null;
            }
            thread["turns"] = json!([turn]);
            json!({
                "thread": thread,
                "threadId": thread_id(request),
                "model": "gpt-5.1-codex",
                "reasoningEffort": "medium",
                "serviceTier": Value::Null,
                "cwd": "/tmp/mock-work"
            })
        }
        "thread/unsubscribe" => json!({}),
        "thread/inject_items" if thread_id(request) == "thread_invalid_inject_result" => {
            Value::Null
        }
        "thread/inject_items" => json!({}),
        "thread/settings/update" => json!({}),
        "thread/loaded/list" => page(json!(["thread_1"])),
        "turn/steer" if thread_id(request) == "thread_mismatched_steer_result" => {
            json!({"turnId": "turn_other"})
        }
        "turn/steer" if thread_id(request) == "thread_invalid_steer_result" => {
            json!({"turnId": 7})
        }
        "turn/steer" => {
            json!({ "turnId": request["params"]["expectedTurnId"].as_str().unwrap_or("turn_1") })
        }
        "turn/interrupt" if thread_id(request) == "thread_invalid_interrupt_result" => Value::Null,
        "turn/interrupt" => json!({}),
        "thread/archive" => json!({}),
        "thread/unarchive" => json!({ "thread": sample_thread(thread_id(request)) }),
        "thread/delete" => json!({}),
        "model/list" => page(json!([{ "id": "gpt-5.5", "name": "GPT-5.5" }])),
        "account/rateLimits/read" => sample_usage(),
        "account/rateLimitResetCredit/consume" => json!({ "outcome": "reset" }),
        "thread/goal/get" => {
            json!({ "goal": goal_to_value(thread_id(request), &goal_for_thread(request, goal_state)) })
        }
        "thread/goal/set" => json!({
            "goal": goal_to_value(thread_id(request), &set_goal_for_thread(request, goal_state))
        }),
        "thread/goal/clear" => {
            if let Some(thread_id) = request["params"]["threadId"].as_str() {
                goal_state.lock().expect("goal state").remove(thread_id);
            }
            json!({ "cleared": true })
        }
        other => panic!("unexpected method {other}"),
    }
}

fn goal_for_thread(
    request: &Value,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> GoalState {
    let thread_id = request["params"]["threadId"].as_str().unwrap_or("thread_1");
    let mut goals = goal_state.lock().expect("goal state");
    goals.entry(thread_id.to_string()).or_default().clone()
}

fn set_goal_for_thread(
    request: &Value,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> GoalState {
    let thread_id = request["params"]["threadId"].as_str().unwrap_or("thread_1");
    let mut goals = goal_state.lock().expect("goal state");
    let goal = goals.entry(thread_id.to_string()).or_default();
    if let Some(objective) = request["params"]["objective"].as_str() {
        goal.objective = objective.to_string();
    }
    if let Some(status) = request["params"]["status"].as_str() {
        goal.status = status.to_string();
    }
    if let Some(token_budget) = request["params"]["tokenBudget"].as_i64() {
        goal.token_budget = token_budget;
    }
    goal.clone()
}

fn goal_to_value(thread_id: &str, goal: &GoalState) -> Value {
    json!({
        "threadId": thread_id,
        "objective": goal.objective,
        "status": goal.status,
        "tokenBudget": goal.token_budget,
        "tokensUsed": 0,
        "timeUsedSeconds": 0,
        "createdAt": 1,
        "updatedAt": 1,
    })
}

fn page(data: Value) -> Value {
    json!({ "data": data, "nextCursor": Value::Null, "backwardsCursor": Value::Null })
}

fn paged_threads(request: &Value) -> Value {
    match request["params"]["cursor"].as_str() {
        None => json!({
            "data": [sample_thread_with_updated("thread_old", 1_600_000_000)],
            "nextCursor": "page2",
            "backwardsCursor": Value::Null
        }),
        Some("page2") => json!({
            "data": [
                sample_thread_with_updated("thread_new_1", 1_700_000_100),
                sample_thread_with_updated("thread_new_2", 1_700_000_200)
            ],
            "nextCursor": "page3",
            "backwardsCursor": Value::Null
        }),
        _ => page(json!([])),
    }
}

// Genuinely descending-by-updatedAt pages. `spage2` opens with a thread older
// than the test cutoff, so a sort-aware `--since` scan should stop there and
// never request `spage3` (whose "tripwire" thread is newer than the cutoff and
// would wrongly appear if paging continued past the boundary).
fn sorted_desc_threads(request: &Value) -> Value {
    match request["params"]["cursor"].as_str() {
        None => json!({
            "data": [
                sample_thread_with_updated("thread_s1", 1_700_000_300),
                sample_thread_with_updated("thread_s2", 1_700_000_200)
            ],
            "nextCursor": "spage2",
            "backwardsCursor": Value::Null
        }),
        Some("spage2") => json!({
            "data": [sample_thread_with_updated("thread_s_old", 1_600_000_000)],
            "nextCursor": "spage3",
            "backwardsCursor": Value::Null
        }),
        Some("spage3") => json!({
            "data": [sample_thread_with_updated("thread_s_tripwire", 1_700_000_999)],
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        }),
        _ => page(json!([])),
    }
}

fn paged_search_results(request: &Value) -> Value {
    let page = paged_threads(request);
    let data = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| json!({ "thread": thread, "score": 1.0 }))
        .collect::<Vec<_>>();
    json!({
        "data": data,
        "nextCursor": page["nextCursor"].clone(),
        "backwardsCursor": page["backwardsCursor"].clone()
    })
}

fn paged_turn_results(request: &Value) -> Value {
    match request["params"]["cursor"].as_str() {
        None => {
            let turns = (0..100)
                .map(|index| {
                    json!({
                        "id": format!("turn_recent_{index}"),
                        "status": "completed",
                        "items": []
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "data": turns,
                "nextCursor": "turn-page-2",
                "backwardsCursor": Value::Null
            })
        }
        Some("turn-page-2") => {
            let mut turn = sample_turn();
            turn["id"] = json!("turn_target_101");
            page(json!([turn]))
        }
        _ => page(json!([])),
    }
}

fn thread_id(request: &Value) -> &str {
    request["params"]["threadId"].as_str().unwrap_or("thread_1")
}

fn sample_usage() -> Value {
    json!({
        "rateLimits": {
            "limitId": "codex",
            "limitName": "Codex",
            "primary": {
                "usedPercent": 37,
                "windowDurationMins": 300,
                "resetsAt": 1700000000
            },
            "secondary": {
                "usedPercent": 12,
                "windowDurationMins": 10080,
                "resetsAt": 1700600000
            },
            "credits": {
                "hasCredits": true,
                "unlimited": false,
                "balance": "42.50"
            },
            "planType": "pro",
            "rateLimitReachedType": null
        },
        "rateLimitResetCredits": {
            "availableCount": 2,
            "credits": [
                {
                    "id": "credit_later",
                    "status": "available",
                    "grantedAt": 100,
                    "expiresAt": 1_800_000_000,
                    "title": "Later reset"
                },
                {
                    "id": "credit_soonest",
                    "status": "available",
                    "grantedAt": 200,
                    "expiresAt": 1_700_000_000,
                    "title": "Soonest reset"
                }
            ]
        },
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "limitName": "Codex",
                "primary": {
                    "usedPercent": 37,
                    "windowDurationMins": 300,
                    "resetsAt": 1700000000
                },
                "secondary": {
                    "usedPercent": 12,
                    "windowDurationMins": 10080,
                    "resetsAt": 1700600000
                },
                "credits": {
                    "hasCredits": true,
                    "unlimited": false,
                    "balance": "42.50"
                },
                "planType": "pro",
                "rateLimitReachedType": null
            },
            "priority": {
                "limitId": "priority",
                "limitName": "Priority",
                "primary": {
                    "usedPercent": 65,
                    "windowDurationMins": 1440,
                    "resetsAt": 1700100000
                },
                "secondary": null,
                "credits": null,
                "planType": "pro",
                "rateLimitReachedType": "rate_limit_reached"
            }
        }
    })
}

fn sample_thread(id: &str) -> Value {
    sample_thread_with_updated(id, 1_700_000_100)
}

fn sample_thread_with_updated(id: &str, updated_at: i64) -> Value {
    json!({
        "id": id,
        "name": "Mock Thread",
        "preview": "Mock preview",
        "cwd": "/tmp/mock-work",
        "status": { "type": "idle" },
        "createdAt": 1_700_000_000_i64,
        "updatedAt": updated_at,
        "experimentalThreadField": {
            "retained": true
        }
    })
}

fn sample_thread_with_parent(id: &str, parent_id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["parentThreadId"] = json!(parent_id);
    thread
}

fn sample_pinned_thread(id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["isPinned"] = json!(true);
    thread
}

fn sample_forked_thread(id: &str, source_id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["forkedFromId"] = json!(source_id);
    thread
}

fn sample_multiline_preview_thread(id: &str) -> Value {
    json!({
        "id": id,
        "name": Value::Null,
        "preview": "First line of a very long preview\nsecond line\twith a tab and enough text to force truncation because this should not spill across terminal rows",
        "cwd": "/tmp/mock-work",
        "status": { "type": "notLoaded" },
        "createdAt": 1_700_000_000_i64,
        "updatedAt": 1_700_000_100_i64
    })
}

fn sample_turn() -> Value {
    json!({
        "id": "turn_1",
        "status": "completed",
        "startedAt": 1_700_000_050_i64,
        "completedAt": 1_700_000_060_i64,
        "experimentalTurnField": "retained",
        "items": [
            {
                "id": "item_user",
                "type": "userMessage",
                "content": [{ "type": "text", "text": "hello" }]
            },
            {
                "id": "item_agent",
                "type": "agentMessage",
                "text": "done"
            }
        ]
    })
}

fn run_json(server: &MockServer, args: &[&str]) -> Value {
    let output = server
        .command()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("json output")
}

fn run_json_with_state(server: &MockServer, state: &TempDir, args: &[&str]) -> Value {
    let output = server
        .command()
        .env("CODEX_TAMER_STATE", state.path())
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("json output")
}

fn run_ndjson(server: &MockServer, args: &[&str]) -> Vec<Value> {
    let output = server
        .command()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ndjson"))
        .collect()
}

fn run_and_interrupt(server: &MockServer, args: &[&str]) -> std::process::Output {
    let child = server
        .std_command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn command");
    thread::sleep(std::time::Duration::from_millis(1_500));
    let signal = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal.success());
    child
        .wait_with_output()
        .expect("wait for interrupted command")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
}

fn write_config(server: &MockServer, contents: impl AsRef<str>) {
    fs::write(&server.config, contents.as_ref()).expect("config");
}

fn assert_thread_yolo_params(params: &Value) {
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandbox"], "danger-full-access");
}

fn assert_turn_yolo_params(params: &Value) {
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
}

fn assert_no_yolo_params(params: &Value) {
    assert!(params.get("approvalPolicy").is_none());
    assert!(params.get("sandbox").is_none());
    assert!(params.get("sandboxPolicy").is_none());
}

#[test]
fn connect_bypasses_config_and_lists_threads() {
    let server = MockServer::start();
    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(server.endpoint())
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["server"], server.endpoint());
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn connect_bypasses_config_for_servers_ping() {
    let server = MockServer::start();
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(server.endpoint())
        .args(["servers", "ping"])
        .assert()
        .success()
        .stdout(predicates::str::contains("SERVER"))
        .stdout(predicates::str::contains("STATUS"))
        .stdout(predicates::str::contains(server.endpoint()))
        .stdout(predicates::str::contains("ok"));
}

#[test]
fn connect_servers_listing_does_not_invent_a_managed_target() {
    let server = MockServer::start();
    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg(server.endpoint())
        .args(["servers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value, json!({"servers": []}));
}

#[test]
fn explicitly_selected_missing_config_paths_are_errors() {
    let temp = TempDir::new().expect("tempdir");
    let missing_flag = temp.path().join("missing-flag.toml");
    let missing_env = temp.path().join("missing-env.toml");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--config")
        .arg(&missing_flag)
        .args(["servers", "--json"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(format!(
            "failed to read config `{}`",
            missing_flag.display()
        )));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env("CODEX_TAMER_CONFIG", &missing_env)
        .env_remove("CODEX_TAMER_SERVER")
        .args(["servers", "--json"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(format!(
            "failed to read config `{}`",
            missing_env.display()
        )));
}

#[test]
fn connect_ws_bypasses_config_and_lists_threads() {
    let server = TcpMockServer::start(None);
    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(&server.endpoint)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["server"], server.endpoint);
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn configured_ws_server_lists_threads() {
    let server = TcpMockServer::start(None);
    let value = server
        .command()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&value).expect("json output");
    assert_eq!(value["server"], "work");
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn configured_ws_server_sends_literal_auth_token() {
    let server = TcpMockServer::start(Some("secret-token"));
    server
        .command()
        .args(["models", "--json"])
        .assert()
        .success();
}

#[test]
fn servers_listing_does_not_resolve_auth_token_env() {
    let server = MockServer::start();
    write_config(
        &server,
        format!(
            r#"[servers.local]
endpoint = "{}"

[servers.remote]
endpoint = "ws://127.0.0.1:9"
auth_token_env = "CODEX_TAMER_MISSING_TOKEN"
"#,
            server.endpoint()
        ),
    );

    let output = server
        .command()
        .env_remove("CODEX_TAMER_MISSING_TOKEN")
        .args(["servers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["servers"].as_array().unwrap().len(), 2);
    assert_eq!(value["servers"][0]["alias"], "local");
    assert_eq!(value["servers"][1]["alias"], "remote");
    assert_eq!(value["servers"][1]["endpoint"], "ws://127.0.0.1:9/");
}

#[test]
fn missing_config_reuses_the_codex_home_managed_listener() {
    let server = MockServer::start_managed();

    let listing = server
        .managed_command()
        .args(["servers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listing: Value = serde_json::from_slice(&listing).expect("server listing");
    assert_eq!(listing["servers"][0]["alias"], "managed");
    assert_eq!(listing["servers"][0]["endpoint"], server.endpoint());
    assert_eq!(listing["servers"][0]["managed"], true);

    let threads = server
        .managed_command()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let threads: Value = serde_json::from_slice(&threads).expect("thread list");
    assert_eq!(threads["server"], "managed");
    assert_eq!(threads["threads"][0]["id"], "thread_1");

    server
        .managed_command()
        .args(["list", "--server", "managed", "--json"])
        .assert()
        .success();

    server
        .managed_command()
        .args(["servers", "ping", "--json"])
        .assert()
        .success();

    let started = server
        .managed_command()
        .args(["servers", "start", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let started: Value = serde_json::from_slice(&started).expect("start report");
    assert_eq!(started["server"], "managed");
    assert_eq!(started["status"], "reused");
    assert_eq!(started["backend"], "external");

    let status = server
        .managed_command()
        .args(["servers", "status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status).expect("status report");
    assert_eq!(status["server"], "managed");
    assert_eq!(status["running"], true);
}

#[test]
fn managed_status_reports_stopped_only_for_an_absent_listener() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let user_home = temp.path().join("home");
    let codex_home = temp.path().join("codex-home");
    let runtime = temp.path().join("runtime");
    fs::create_dir(&user_home).expect("user home");
    fs::create_dir(&codex_home).expect("codex home");
    fs::create_dir(&runtime).expect("runtime");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");

    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args(["servers", "status", "--json"])
        .assert()
        .success()
        .stderr(predicates::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&output).expect("status json");

    assert_eq!(output["status"], "stopped");
    assert_eq!(output["running"], false);
    assert!(output.get("error").is_none());
}

#[test]
fn incompatible_existing_managed_listener_fails_without_invoking_codex() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start_managed_with_user_agent("codex_cli_rs/0.147.0 (test)");
    let invoked = server._temp.path().join("codex-invoked");
    let fake_codex = server._temp.path().join("codex");
    fs::write(
        &fake_codex,
        format!(
            "#!/bin/sh\nprintf 'invoked\\n' > '{}'\nexit 1\n",
            invoked.display()
        ),
    )
    .expect("fake codex");
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).expect("fake codex mode");

    server
        .managed_command()
        .args(["servers", "status", "--json"])
        .assert()
        .code(3)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains(
            "app-server version `0.147.0` is incompatible",
        ));

    server
        .managed_command()
        .arg("--codex")
        .arg(&fake_codex)
        .args(["servers", "start", "--json"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "app-server version `0.147.0` is incompatible",
        ));

    assert!(
        !invoked.exists(),
        "an incompatible reachable listener must not trigger Codex startup"
    );
}

#[test]
fn explicit_managed_target_with_configured_servers_keeps_managed_validation() {
    let server = MockServer::start_managed_with_user_agent("codex_cli_rs/0.147.0 (test)");
    fs::create_dir_all(server.config.parent().expect("config parent")).expect("config parent");
    fs::write(
        &server.config,
        "[servers.work]\nendpoint = \"ws://127.0.0.1:9\"\n",
    )
    .expect("config");

    server
        .managed_command()
        .args(["list", "--server", "managed", "--json"])
        .assert()
        .code(3)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains(
            "app-server version `0.147.0` is incompatible",
        ));
}

#[test]
fn malformed_reachable_managed_listener_fails_without_invoking_codex() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start_managed_with_initialize_override(
        "codex_cli_rs/0.146.0 (test)",
        Some(json!({"unexpected": true})),
    );
    let invoked = server._temp.path().join("codex-invoked");
    let fake_codex = server._temp.path().join("codex");
    fs::write(
        &fake_codex,
        format!(
            "#!/bin/sh\nprintf 'invoked\\n' > '{}'\nexit 1\n",
            invoked.display()
        ),
    )
    .expect("fake codex");
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).expect("fake codex mode");

    server
        .managed_command()
        .args(["servers", "status", "--json"])
        .assert()
        .code(3)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains(
            "initialize response has an invalid result",
        ));

    server
        .managed_command()
        .arg("--codex")
        .arg(&fake_codex)
        .args(["servers", "start", "--json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "initialize response has an invalid result",
        ));

    assert!(
        !invoked.exists(),
        "a reachable listener with a malformed handshake must not trigger Codex startup"
    );
}

#[test]
fn managed_commands_reject_an_insecure_runtime_tree_before_connecting() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start_managed();
    let managed_root = server
        .managed_runtime
        .as_ref()
        .expect("managed runtime")
        .join("codex-tamer");
    fs::set_permissions(&managed_root, fs::Permissions::from_mode(0o755))
        .expect("insecure managed root mode");

    let list = server
        .managed_command()
        .args(["list", "--json"])
        .output()
        .expect("list output");
    let status = server
        .managed_command()
        .args(["servers", "status", "--json"])
        .output()
        .expect("status output");
    let stop = server
        .managed_command()
        .args(["servers", "stop", "--json"])
        .output()
        .expect("stop output");

    assert!(!list.status.success());
    assert!(
        String::from_utf8_lossy(&list.stderr).contains("must be owned by uid"),
        "list stderr: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(!status.status.success());
    assert!(status.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("must be owned by uid"),
        "status stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!stop.status.success());
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("must be owned by uid"),
        "stop stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

#[test]
fn managed_discovery_ignores_a_stale_server_selection_environment_variable() {
    let server = MockServer::start_managed();

    let listing = server
        .managed_command()
        .env("CODEX_TAMER_SERVER", "stale")
        .args(["servers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listing: Value = serde_json::from_slice(&listing).expect("server listing");
    assert_eq!(listing["servers"][0]["alias"], "managed");

    let ping = server
        .managed_command()
        .env("CODEX_TAMER_SERVER", "stale")
        .args(["servers", "ping", "--all", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let ping: Value = serde_json::from_slice(&ping).expect("ping output");
    assert_eq!(ping["servers"][0], json!({"server": "managed", "ok": true}));
}

#[test]
fn process_record_failure_terminates_the_spawned_app_server() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let runtime = temp.path().join("runtime");
    let user_home = temp.path().join("user-home");
    fs::create_dir_all(&codex_home).expect("codex home");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::create_dir_all(&user_home).expect("user home");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
    let codex_home = fs::canonicalize(codex_home).expect("canonical home");
    let digest = blake3::hash(codex_home.as_os_str().as_encoded_bytes());
    let managed_root = runtime.join("codex-tamer");
    let socket_dir = managed_root.join(&digest.to_hex().as_str()[..24]);
    fs::create_dir_all(&socket_dir).expect("managed socket directory");
    fs::set_permissions(&managed_root, fs::Permissions::from_mode(0o700))
        .expect("managed root mode");
    fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).expect("socket dir mode");

    let lock_path = socket_dir.join("start.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("lifecycle lock");
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).expect("lock mode");
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

    let pid_file = temp.path().join("spawned.pid");
    let descendant_pid_file = temp.path().join("spawned-descendant.pid");
    let fake_codex = temp.path().join("codex");
    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
printf '%s' "$$" > '{}'
/bin/sleep 3 &
descendant=$!
printf '%s' "$descendant" > '{}'
exit 0
"#,
            pid_file.display(),
            descendant_pid_file.display()
        ),
    )
    .expect("fake codex");
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).expect("fake codex mode");

    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("codex-tamer"));
    command
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .arg("--codex")
        .arg(&fake_codex)
        .args(["servers", "start", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command.spawn().expect("codex-tamer process");
    let record_temp = socket_dir.join(format!("process.{}.tmp", child.id()));
    fs::create_dir(&record_temp).expect("blocking process record temporary path");
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);

    let output = child.wait_with_output().expect("codex-tamer output");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to create process record"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_recorded_process_exited(&pid_file, "app-server launcher");
    assert_recorded_process_exited(&descendant_pid_file, "app-server descendant");
}

fn process_exists(pid: libc::pid_t) -> bool {
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn assert_recorded_process_exited(pid_file: &std::path::Path, label: &str) {
    use std::time::{Duration, Instant};

    let marker_deadline = Instant::now() + Duration::from_millis(500);
    while !pid_file.exists() && Instant::now() < marker_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !pid_file.exists() {
        return;
    }
    let pid: libc::pid_t = fs::read_to_string(pid_file)
        .expect("spawned pid")
        .parse()
        .expect("numeric pid");
    let exit_deadline = Instant::now() + Duration::from_millis(500);
    while process_exists(pid) && Instant::now() < exit_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_exists(pid), "{label} pid {pid} survived cleanup");
}

#[test]
fn missing_config_starts_the_selected_codex_listener_with_exact_arguments() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let (runtime, _runtime_temp) = managed_runtime_fixture(&temp);
    let user_home = temp.path().join("user-home");
    fs::create_dir_all(&codex_home).expect("codex home");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::create_dir_all(&user_home).expect("user home");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
    let codex_home = fs::canonicalize(codex_home).expect("canonical home");
    let digest = blake3::hash(codex_home.as_os_str().as_encoded_bytes());
    let socket_dir = runtime
        .join("codex-tamer")
        .join(&digest.to_hex().as_str()[..24]);
    let socket = socket_dir.join("app-server.sock");
    let marker = temp.path().join("spawned.args");
    let observed_home = temp.path().join("spawned.home");
    let launcher_pid = temp.path().join("spawned.launcher.pid");
    let descendant_pid = temp.path().join("spawned.descendant.pid");
    let helper = std::env::current_exe().expect("test helper executable");
    let fake_codex = temp.path().join("codex");
    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
printf '%s\n' "$@" > '{}'
printf '%s' "$CODEX_HOME" > '{}'
printf '%s' "$$" > '{}'
CODEX_TAMER_TEST_MANAGED_SOCKET='{}' \
CODEX_TAMER_TEST_MANAGED_HOME="$CODEX_HOME" \
  '{}' --exact managed_listener_process_helper --ignored --nocapture &
(
  trap 'sleep 1; exit 0' TERM INT HUP
  while :; do sleep 10; done
) &
printf '%s' "$!" > '{}'
sleep 0.3
exit 0
"#,
            marker.display(),
            observed_home.display(),
            launcher_pid.display(),
            socket.display(),
            helper.display(),
            descendant_pid.display()
        ),
    )
    .expect("fake codex");
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).expect("fake codex mode");

    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .arg("--codex")
        .arg(&fake_codex)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&output).expect("list output");
    assert_eq!(output["server"], "managed");
    assert_eq!(
        fs::read_to_string(&marker).expect("spawn args"),
        format!("app-server\n--listen\nunix://{}\n", socket.display())
    );
    assert_eq!(
        fs::read_to_string(&observed_home).expect("spawn home"),
        codex_home.to_string_lossy()
    );

    let process_record = socket_dir.join("process.json");
    assert!(process_record.exists());
    assert_recorded_process_exited(&launcher_pid, "app-server launcher");
    let status = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("TZ", "Pacific/Honolulu")
        .env("PATH", "")
        .args(["servers", "status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status).expect("status report");
    assert_eq!(status["backend"], "codex-tamer");
    assert!(
        process_record.exists(),
        "status must not discard a live process record when TZ or PATH changes"
    );

    let stopped = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .arg("--codex")
        .arg(&fake_codex)
        .args(["servers", "stop", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stopped: Value = serde_json::from_slice(&stopped).expect("stop report");
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["backend"], "codex-tamer");
    assert_eq!(stopped["running"], false);
    assert!(!process_record.exists());
    assert_recorded_process_exited(&descendant_pid, "app-server descendant");
}

#[test]
fn concurrent_first_start_spawns_one_listener_and_reuses_it() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let (runtime, _runtime_temp) = managed_runtime_fixture(&temp);
    let user_home = temp.path().join("user-home");
    fs::create_dir_all(&codex_home).expect("codex home");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::create_dir_all(&user_home).expect("user home");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
    let codex_home = fs::canonicalize(codex_home).expect("canonical home");
    let digest = blake3::hash(codex_home.as_os_str().as_encoded_bytes());
    let socket_dir = runtime
        .join("codex-tamer")
        .join(&digest.to_hex().as_str()[..24]);
    let socket = socket_dir.join("app-server.sock");
    let launches = temp.path().join("launches.log");
    let helper = std::env::current_exe().expect("test helper executable");
    let fake_codex = temp.path().join("codex");
    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
printf 'launch\n' >> '{}'
CODEX_TAMER_TEST_MANAGED_SOCKET='{}' \
CODEX_TAMER_TEST_MANAGED_HOME="$CODEX_HOME" \
  exec '{}' --exact managed_listener_process_helper --ignored --nocapture
"#,
            launches.display(),
            socket.display(),
            helper.display()
        ),
    )
    .expect("fake codex");
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).expect("fake codex mode");

    let command = || {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("codex-tamer"));
        command
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .env("HOME", &user_home)
            .env("CODEX_HOME", &codex_home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .arg("--codex")
            .arg(&fake_codex)
            .args(["list", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    };
    let first = command().spawn().expect("first codex-tamer");
    let second = command().spawn().expect("second codex-tamer");
    let first = first.wait_with_output().expect("first output");
    let second = second.wait_with_output().expect("second output");

    let stop = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("HOME", &user_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .arg("--codex")
        .arg(&fake_codex)
        .args(["servers", "stop", "--json"])
        .assert()
        .success();
    drop(stop);

    assert!(
        first.status.success(),
        "first stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "second stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read_to_string(&launches)
            .expect("launch count")
            .lines()
            .count(),
        1
    );
    assert!(!socket_dir.join("process.json").exists());
}

#[test]
fn servers_stop_refuses_a_reachable_external_listener_without_a_process_record() {
    let server = MockServer::start_managed();

    server
        .managed_command()
        .args(["servers", "stop", "--json"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "reachable but is not owned by codex-tamer; refusing to stop it",
        ));

    server
        .managed_command()
        .args(["list", "--json"])
        .assert()
        .success();
}

#[test]
fn servers_stop_rejects_a_malformed_reachable_listener_without_a_process_record() {
    let server = MockServer::start_managed_with_initialize_override(
        "codex_cli_rs/0.146.0 (test)",
        Some(json!({"unexpected": true})),
    );

    server
        .managed_command()
        .args(["servers", "stop", "--json"])
        .assert()
        .code(3)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains(
            "endpoint is reachable but cannot be verified for shutdown",
        ));
}

#[test]
fn servers_ping_all_reports_unresolved_auth_token_env_per_server() {
    let server = MockServer::start();
    write_config(
        &server,
        format!(
            r#"[servers.local]
endpoint = "{}"

[servers.remote]
endpoint = "ws://127.0.0.1:9"
auth_token_env = "CODEX_TAMER_MISSING_TOKEN"
"#,
            server.endpoint()
        ),
    );

    let output = server
        .command()
        .env_remove("CODEX_TAMER_MISSING_TOKEN")
        .args(["servers", "ping", "--all", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["servers"][0], json!({"server": "local", "ok": true}));
    assert_eq!(
        value["servers"][1],
        json!({"server": "remote", "ok": false})
    );
}

#[test]
fn connect_ws_sends_literal_auth_token() {
    let server = TcpMockServer::start(Some("direct-token"));
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg(&server.endpoint)
        .arg("--connect-auth-token")
        .arg("direct-token")
        .args(["models", "--json"])
        .assert()
        .success();
}

#[test]
fn connect_ws_sends_env_auth_token() {
    let server = TcpMockServer::start(Some("env-token"));
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .env("CODEX_TAMER_TEST_TOKEN", "env-token")
        .arg("--connect")
        .arg(&server.endpoint)
        .arg("--connect-auth-token-env")
        .arg("CODEX_TAMER_TEST_TOKEN")
        .args(["models", "--json"])
        .assert()
        .success();
}

#[test]
fn connect_rejects_servers_ping_all() {
    let server = MockServer::start();
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg(server.endpoint())
        .args(["servers", "ping", "--all"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "--connect cannot be combined with servers ping --all",
        ));
}

#[test]
fn connect_auth_flags_require_websocket_endpoint() {
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg("unix:///tmp/missing.sock")
        .arg("--connect-auth-token")
        .arg("secret")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "auth token requires a websocket endpoint",
        ));
}

#[test]
fn connect_auth_flags_reject_non_loopback_plain_ws() {
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg("ws://example.com:8765")
        .arg("--connect-auth-token")
        .arg("secret")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("wss:// or loopback ws://"));
}

#[test]
fn connect_auth_flags_are_mutually_exclusive() {
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--connect")
        .arg("ws://127.0.0.1:8765")
        .arg("--connect-auth-token")
        .arg("secret")
        .arg("--connect-auth-token-env")
        .arg("CODEX_TAMER_TEST_TOKEN")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn missing_server_is_an_error_when_multiple_servers_are_configured() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        r#"
[servers.one]
type = "uds"
path = "/tmp/one.sock"

[servers.two]
type = "uds"
path = "/tmp/two.sock"
"#,
    )
    .expect("config");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env_remove("CODEX_TAMER_CONFIG")
        .env_remove("CODEX_TAMER_SERVER")
        .arg("--config")
        .arg(config)
        .args(["list", "--json"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("multiple servers configured"));
}

#[test]
fn completion_commands_print_setup_scripts_and_candidates() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        r#"
[servers.work]
type = "uds"
path = "/tmp/work.sock"

[servers.personal]
type = "uds"
path = "/tmp/personal.sock"
"#,
    )
    .expect("config");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .env("SHELL", "/bin/bash")
        .args(["completion"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Detected shell: bash"))
        .stdout(predicates::str::contains(
            "source <(codex-tamer completion script bash)",
        ));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["completion", "script", "bash"])
        .assert()
        .success()
        .stdout(predicates::str::contains("while IFS= read -r candidate"))
        .stdout(predicates::str::contains("COMPREPLY+=(\"$candidate\")"))
        .stdout(predicates::str::contains(
            "complete -o bashdefault -o default -F _codex_tamer_completion codex-tamer",
        ))
        .stdout(predicates::str::contains(
            "codex-tamer __complete -- \"$cur\"",
        ));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["completion", "script", "zsh"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "compdef _codex_tamer codex-tamer",
        ))
        .stdout(predicates::str::contains("_files"))
        .stdout(predicates::str::contains(
            "codex-tamer __complete -- \"$current\"",
        ));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["completion", "script", "fish"])
        .assert()
        .success()
        .stdout(predicates::str::contains("complete -c codex-tamer -a"))
        .stdout(predicates::str::contains(
            "codex-tamer __complete -- \"$current\"",
        ));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["__complete", "--", "l"])
        .assert()
        .success()
        .stdout(predicates::str::contains("list\n"));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["__complete", "--", "p", "servers"])
        .assert()
        .success()
        .stdout("ping\n");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["__complete", "--", "--so", "list"])
        .assert()
        .success()
        .stdout("--source\n--sort\n");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["__complete", "--", "u", "list", "--sort"])
        .assert()
        .success()
        .stdout("updated\n");

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args([
            "__complete",
            "--",
            "wo",
            "--config",
            config.to_str().expect("utf8 path"),
            "list",
            "--server",
        ])
        .assert()
        .success()
        .stdout("work\n");

    let bash_completion = |words: &[&str], cword: usize| -> String {
        let binary = assert_cmd::cargo::cargo_bin("codex-tamer");
        let binary_dir = binary.parent().expect("binary parent");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(binary_dir.to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .expect("join path");
        let words = words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "source <(codex-tamer completion script bash); \
             COMP_WORDS=({words}); \
             COMP_CWORD={cword}; \
             _codex_tamer_completion; \
             printf '%s\\n' \"${{COMPREPLY[@]}}\""
        );
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .env("PATH", path)
            .env_remove("CODEX_TAMER_CONFIG")
            .env_remove("CODEX_TAMER_SERVER")
            .output()
            .expect("run bash completion smoke");
        assert!(
            output.status.success(),
            "bash completion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 stdout")
    };

    assert_eq!(bash_completion(&["codex-tamer", "l"], 1), "list\n");
    assert_eq!(
        bash_completion(&["codex-tamer", "servers", "p"], 2),
        "ping\n"
    );
    assert_eq!(
        bash_completion(&["codex-tamer", "list", "--so"], 2),
        "--source\n--sort\n"
    );
    assert_eq!(
        bash_completion(&["codex-tamer", "list", "--sort", "u"], 3),
        "updated\n"
    );
    assert_eq!(
        bash_completion(&["codex-tamer", "list", "--sort=u"], 2),
        "--sort=updated\n"
    );
    assert_eq!(
        bash_completion(
            &[
                "codex-tamer",
                "--config",
                config.to_str().expect("utf8 path"),
                "list",
                "--server",
                "wo",
            ],
            5,
        ),
        "work\n"
    );
    assert!(!bash_completion(&["codex-tamer", ""], 1).contains("__complete"));

    let marker = temp.path().join("completion-pwned");
    let malicious_alias = format!("$(touch {})", marker.display());
    fs::write(
        &config,
        format!(
            r#"
[servers.work]
type = "uds"
path = "/tmp/work.sock"

[servers.{malicious_alias}]
type = "uds"
path = "/tmp/malicious.sock"
"#,
            malicious_alias = toml_string(&malicious_alias),
        ),
    )
    .expect("config");

    assert_eq!(
        bash_completion(
            &[
                "codex-tamer",
                "--config",
                config.to_str().expect("utf8 path"),
                "list",
                "--server",
                "$",
            ],
            5,
        ),
        format!("{malicious_alias}\n")
    );
    assert!(
        !marker.exists(),
        "completion candidate executed as shell code"
    );
}

#[test]
fn clap_value_parsers_reject_empty_static_values_before_connecting() {
    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["new", "--cwd", ".", "--effort", " "])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "reasoning effort cannot be empty",
        ));

    Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["goal", "set", "thread_1", "--status", "finished"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("invalid value"));
}

#[test]
fn read_only_commands_return_scriptable_json() {
    let server = MockServer::start();

    assert_eq!(
        run_json(&server, &["servers", "--json"])["servers"][0]["alias"],
        "work"
    );
    assert_eq!(
        run_json(&server, &["servers", "--json"])["servers"][0]["endpoint"],
        server.endpoint()
    );
    assert_eq!(
        run_json(&server, &["servers", "ping", "--server", "work", "--json"])["servers"][0]["ok"],
        true
    );
    assert_eq!(
        run_json(&server, &["list", "--server", "work", "--json"])["threads"][0]["id"],
        "thread_1"
    );
    assert_eq!(
        run_json(
            &server,
            &["search", "threads", "--server", "work", "--json", "mock"]
        )["results"][0]["thread"]["id"],
        "thread_1"
    );
    assert_eq!(
        run_json(&server, &["show", "--server", "work", "--json", "thread_1"])["turns"]["data"][0]
            ["id"],
        "turn_1"
    );
    assert_eq!(
        run_json(
            &server,
            &["messages", "--server", "work", "--json", "thread_1"]
        )["messages"][1]["role"],
        "assistant"
    );
    let user_messages = run_json(
        &server,
        &[
            "messages", "--server", "work", "--json", "--role", "user", "thread_1",
        ],
    );
    assert_eq!(user_messages["messages"].as_array().unwrap().len(), 1);
    assert_eq!(user_messages["messages"][0]["role"], "user");
    assert_eq!(
        run_json(&server, &["status", "--server", "work", "--json"])["loadedThreadIds"][0],
        "thread_1"
    );
    assert_eq!(
        run_json(
            &server,
            &["status", "--server", "work", "--json", "thread_1"]
        )["threadId"],
        "thread_1"
    );
    assert!(
        !server
            .methods()
            .iter()
            .any(|method| method == "thread/resume"),
        "plain status should not resume/load threads"
    );
    assert_eq!(
        run_json(&server, &["models", "--server", "work", "--json"])["models"][0]["id"],
        "gpt-5.5"
    );
    let usage = run_json(&server, &["usage", "--server", "work", "--json"]);
    assert_eq!(usage["server"], "work");
    assert_eq!(usage["rateLimits"]["credits"]["balance"], "42.50");
    assert_eq!(usage["rateLimitResetCredits"]["availableCount"], 2);
    assert_eq!(
        usage["rateLimitsByLimitId"]["priority"]["rateLimitReachedType"],
        "rate_limit_reached"
    );
}

#[test]
fn annotation_commands_manage_local_state_without_app_server() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");

    let set = run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "Release follow-up",
        ],
    );
    assert_eq!(set["server"], "work");
    assert_eq!(set["threadId"], "thread_1");
    assert_eq!(set["annotation"]["text"], "Release follow-up");
    assert!(server.methods().is_empty());

    let get = run_json_with_state(
        &server,
        &state,
        &["annotate", "get", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(get["annotation"]["text"], "Release follow-up");

    let listed = run_json_with_state(
        &server,
        &state,
        &["annotate", "list", "--server", "work", "--json"],
    );
    assert_eq!(listed["annotations"][0]["threadId"], "thread_1");

    let searched = run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "search", "--server", "work", "--json", "release",
        ],
    );
    assert_eq!(
        searched["annotations"][0]["annotation"]["text"],
        "Release follow-up"
    );

    server
        .command()
        .env("CODEX_TAMER_STATE", state.path())
        .args(["annotate", "get", "--server", "work", "--json", "missing"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("annotation not found"));

    let cleared = run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "clear", "--server", "work", "--json", "thread_1",
        ],
    );
    assert_eq!(cleared["cleared"], true);
    assert!(
        run_json_with_state(
            &server,
            &state,
            &["annotate", "list", "--server", "work", "--json"]
        )["annotations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn annotations_project_into_list_search_and_show_outputs() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "Release follow-up",
        ],
    );

    let listed = run_json_with_state(&server, &state, &["list", "--server", "work", "--json"]);
    assert_eq!(
        listed["threads"][0]["annotation"]["text"],
        "Release follow-up"
    );

    let searched = run_json_with_state(
        &server,
        &state,
        &["search", "threads", "--server", "work", "--json", "mock"],
    );
    assert_eq!(
        searched["results"][0]["thread"]["annotation"]["text"],
        "Release follow-up"
    );

    let shown = run_json_with_state(
        &server,
        &state,
        &["show", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(shown["thread"]["annotation"]["text"], "Release follow-up");

    let output = server
        .command()
        .env("CODEX_TAMER_STATE", state.path())
        .args(["list", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.lines().next().unwrap().contains("ANNOTATION"));
    assert!(text.contains("Release follow-up"));
}

#[test]
fn annotation_prune_removes_only_missing_threads() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "set", "--server", "work", "--json", "thread_1", "Keep",
        ],
    );
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_missing",
            "Remove",
        ],
    );

    let dry_run = run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "prune",
            "--server",
            "work",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(dry_run["checked"], 2);
    assert_eq!(dry_run["stale"], json!(["thread_missing"]));
    assert_eq!(dry_run["removed"], 0);
    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &[
                "annotate",
                "get",
                "--server",
                "work",
                "--json",
                "thread_missing"
            ]
        )["annotation"]["text"],
        "Remove"
    );

    let pruned = run_json_with_state(
        &server,
        &state,
        &["annotate", "prune", "--server", "work", "--json"],
    );
    assert_eq!(pruned["removed"], 1);
    server
        .command()
        .env("CODEX_TAMER_STATE", state.path())
        .args([
            "annotate",
            "get",
            "--server",
            "work",
            "--json",
            "thread_missing",
        ])
        .assert()
        .code(2);
    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &["annotate", "get", "--server", "work", "--json", "thread_1"]
        )["annotation"]["text"],
        "Keep"
    );
}

#[test]
fn annotation_prune_aborts_on_unexpected_thread_read_error() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_error",
            "Keep despite transient error",
        ],
    );

    server
        .command()
        .env("CODEX_TAMER_STATE", state.path())
        .args(["annotate", "prune", "--server", "work", "--json"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("temporary read failure"));

    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &[
                "annotate",
                "get",
                "--server",
                "work",
                "--json",
                "thread_error"
            ]
        )["annotation"]["text"],
        "Keep despite transient error"
    );
}

#[test]
fn status_load_requires_thread_id() {
    let server = MockServer::start();
    server
        .command()
        .args(["status", "--load"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("<THREAD_ID>"));

    assert!(server.methods().is_empty());
}

#[test]
fn status_load_resumes_then_reports_thread_status() {
    let server = MockServer::start();
    let status = run_json(
        &server,
        &["status", "--server", "work", "--json", "--load", "thread_1"],
    );

    assert_eq!(status["threadId"], "thread_1");

    let methods = server.methods();
    let status_methods = methods
        .iter()
        .filter(|method| {
            matches!(
                method.as_str(),
                "thread/resume" | "thread/unsubscribe" | "thread/read" | "thread/turns/list"
            )
        })
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        status_methods,
        [
            "thread/resume",
            "thread/unsubscribe",
            "thread/read",
            "thread/turns/list"
        ]
    );

    let resume_params = server.params_for("thread/resume");
    assert_eq!(resume_params.len(), 1);
    assert_eq!(resume_params[0]["threadId"], "thread_1");
    assert_eq!(resume_params[0]["excludeTurns"], true);
    assert_no_yolo_params(&resume_params[0]);
}

#[test]
fn messages_human_output_uses_readable_blocks() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["messages", "--server", "work", "thread_1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains(" user\nhello"));
    assert!(text.contains("\n\n"));
    assert!(text.contains(" assistant\ndone"));
}

#[test]
fn messages_role_filter_omits_redundant_role_in_human_output() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["messages", "--server", "work", "--role", "user", "thread_1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("\nhello\n"));
    assert!(!text.contains(" user\n"));
    assert!(!text.contains("assistant"));
    assert!(!text.contains("done"));
}

#[test]
fn usage_human_output_shows_credits_and_limit_windows() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["usage", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("server        work"));
    assert!(text.contains("plan          pro"));
    assert!(
        text.lines()
            .any(|line| { line.split_whitespace().collect::<Vec<_>>() == ["credits", "42.50"] })
    );
    assert!(
        text.lines()
            .any(|line| { line.split_whitespace().collect::<Vec<_>>() == ["resetCredits", "2"] })
    );
    assert!(text.contains("LIMIT"));
    assert!(text.contains("WINDOW"));
    assert!(text.contains("REACHED"));
    assert!(text.contains("Codex"));
    assert!(text.contains("primary"));
    assert!(text.contains("37%"));
    assert!(text.contains("300 mins"));
    assert!(text.contains("Priority"));
    assert!(text.contains("65%"));
    assert!(text.contains("rate_limit_reached"));
}

#[test]
fn usage_redeem_requires_server_permission_without_disclosing_how_to_enable_it() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["usage", "redeem", "--server", "work"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("utf8");

    assert!(stderr.contains("rate-limit reset redemption is not permitted"));
    assert!(!stderr.contains("config"));
    assert!(server.methods().is_empty());
}

#[test]
fn usage_redeem_selects_and_redeems_the_soonest_expiring_credit() {
    let server = MockServer::start();
    server.allow_rate_limit_reset();

    let output = run_json(&server, &["usage", "redeem", "--server", "work", "--json"]);
    assert_eq!(output["outcome"], "reset");
    assert_eq!(output["credit"]["id"], "credit_soonest");
    assert_eq!(output["credit"]["title"], "Soonest reset");

    let params = server.params_for("account/rateLimitResetCredit/consume");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["creditId"], "credit_soonest");
    assert!(
        params[0]["idempotencyKey"]
            .as_str()
            .is_some_and(|key| key.starts_with("codex-tamer-"))
    );
}

#[test]
fn usage_redeem_reports_success_when_the_usage_refresh_fails() {
    let server = MockServer::start_with_usage_refresh_failure();
    server.allow_rate_limit_reset();

    let output = run_json(&server, &["usage", "redeem", "--server", "work", "--json"]);
    assert_eq!(output["outcome"], "reset");
    assert_eq!(output["credit"]["id"], "credit_soonest");
    assert_eq!(output["rateLimits"], Value::Null);
    assert!(
        output["refreshError"]
            .as_str()
            .is_some_and(|error| error.contains("usage refresh unavailable"))
    );
    assert_eq!(
        server
            .params_for("account/rateLimitResetCredit/consume")
            .len(),
        1
    );
}

#[test]
fn list_human_output_uses_compact_aligned_table() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["list", "--server", "work", "--cwd", "/tmp/multiline"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("UPDATED"));
    assert!(lines[0].contains("STATUS"));
    assert!(lines[0].contains("TITLE/PREVIEW"));
    assert!(lines[0].contains("THREAD ID"));
    assert!(lines[1].contains("2023-"));
    assert!(!lines[1].contains("1700000100"));
    assert!(lines[1].contains("First line of a very long preview second line with a ..."));
    assert!(lines[1].contains("..."));
    assert!(lines[1].contains("thread_multiline"));
    assert!(!lines[1].contains('\t'));
}

#[test]
fn messages_help_explains_scan_and_filter_order() {
    let output = Command::cargo_bin("codex-tamer")
        .expect("binary")
        .args(["messages", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("Message selection order"));
    assert!(text.contains("--max-turns is the recent turn scan window"));
    assert!(text.contains("Use --last for the final number of messages"));
    assert!(text.contains("Role filters only see messages inside the scanned turns"));
    assert!(text.contains("There is no messages --first"));
}

#[test]
fn list_since_filters_locally_across_server_pages() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--cwd",
            "/tmp/paged",
            "--limit",
            "2",
            "--since",
            "1700000000",
        ],
    );
    assert_eq!(output["threads"].as_array().unwrap().len(), 2);
    assert_eq!(output["threads"][0]["id"], "thread_new_1");
    assert_eq!(output["threads"][1]["id"], "thread_new_2");
    assert_eq!(output["nextCursor"], "page3");
}

#[test]
fn list_since_stops_paging_at_boundary_when_sorted_updated_desc() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--cwd",
            "/tmp/sorted",
            "--limit",
            "10",
            "--since",
            "1700000000",
            "--sort",
            "updated",
            "--desc",
        ],
    );
    let threads = output["threads"].as_array().unwrap();
    // Stops at the first thread older than `since`; never pages to `spage3`,
    // so the newer-but-later "tripwire" thread must not appear.
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0]["id"], "thread_s1");
    assert_eq!(threads[1]["id"], "thread_s2");
    assert!(
        !threads
            .iter()
            .any(|thread| thread["id"] == "thread_s_tripwire"),
        "early-exit should not reach the tripwire page"
    );
    assert_eq!(output["nextCursor"], "spage3");
}

#[test]
fn search_since_filters_locally_across_server_pages() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "search",
            "threads",
            "--server",
            "work",
            "--json",
            "--limit",
            "2",
            "--since",
            "1700000000",
            "paged",
        ],
    );
    assert_eq!(output["results"].as_array().unwrap().len(), 2);
    assert_eq!(output["results"][0]["thread"]["id"], "thread_new_1");
    assert_eq!(output["results"][1]["thread"]["id"], "thread_new_2");
    assert_eq!(output["nextCursor"], "page3");
}

#[test]
fn message_occurrence_search_is_not_exposed_as_a_cli_command() {
    let server = MockServer::start();
    server
        .command()
        .args(["search", "messages", "thread_1", "release"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "unrecognized subcommand 'messages'",
        ));
    assert!(server.params_for("thread/searchOccurrences").is_empty());
}

#[test]
fn list_can_filter_direct_children_and_descendants() {
    let server = MockServer::start();
    let children = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--parent",
            "thread_parent",
        ],
    );
    assert_eq!(children["threads"][0]["id"], "thread_child_1");
    assert_eq!(children["threads"][0]["parentThreadId"], "thread_parent");

    let descendants = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--ancestor",
            "thread_parent",
        ],
    );
    assert_eq!(descendants["threads"].as_array().unwrap().len(), 2);
    assert_eq!(descendants["threads"][0]["id"], "thread_grandchild_1");
    assert_eq!(
        descendants["threads"][0]["parentThreadId"],
        "thread_child_1"
    );

    let params = server.params_for("thread/list");
    assert_eq!(params[0]["parentThreadId"], "thread_parent");
    assert!(params[0].get("ancestorThreadId").is_none());
    assert_eq!(params[1]["ancestorThreadId"], "thread_parent");
    assert!(params[1].get("parentThreadId").is_none());
}

#[test]
fn list_passes_provider_and_source_filters() {
    let server = MockServer::start();
    let _ = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--provider",
            "openai",
            "--provider",
            "azure",
            "--source",
            "sub-agent",
            "--source",
            "sub-agent-review",
        ],
    );

    let params = server.params_for("thread/list");
    assert_eq!(params[0]["modelProviders"], json!(["openai", "azure"]));
    assert_eq!(
        params[0]["sourceKinds"],
        json!(["subAgent", "subAgentReview"])
    );
}

#[test]
fn list_filters_pinned_state_and_marks_pinned_human_rows() {
    let server = MockServer::start();
    let pinned = run_json(&server, &["list", "--server", "work", "--json", "--pinned"]);
    assert_eq!(pinned["threads"][0]["id"], "thread_pinned");
    assert_eq!(pinned["threads"][0]["isPinned"], true);

    let unpinned = run_json(
        &server,
        &["list", "--server", "work", "--json", "--unpinned"],
    );
    assert_eq!(unpinned["threads"][0]["id"], "thread_unpinned");

    let output = server
        .command()
        .args(["list", "--server", "work", "--pinned"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.lines().next().unwrap_or("").contains("PINNED"));
    assert!(text.contains("yes"));

    let params = server.params_for("thread/list");
    assert_eq!(params[0]["isPinned"], true);
    assert_eq!(params[1]["isPinned"], false);
    assert_eq!(params[2]["isPinned"], true);
}

#[test]
fn new_send_and_settings_commands_return_follow_up_ids() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let created = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--model", "gpt-5.5", "--effort", "medium",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let completed = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--json", "say done",
        ],
    );
    assert_eq!(completed["threadId"], "thread_new");
    assert_eq!(completed["turnId"], "turn_1");
    assert_eq!(completed["finalAssistantText"], "done");

    let accepted = run_json(
        &server,
        &[
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["threadId"], "thread_1");
    assert_eq!(accepted["turnId"], "turn_1");

    let settings = run_json(
        &server,
        &["settings", "show", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(settings["model"], "gpt-5.1-codex");

    let updated = run_json(
        &server,
        &[
            "settings",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "--effort",
            "high",
            "--clear-service-tier",
        ],
    );
    assert_eq!(updated["status"], "accepted");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 2);
    assert_thread_yolo_params(&thread_start_params[0]);
    assert_thread_yolo_params(&thread_start_params[1]);

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert_turn_yolo_params(&turn_start_params[0]);
    assert_turn_yolo_params(&turn_start_params[1]);

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_no_yolo_params(&thread_resume_params[0]);
}

#[test]
fn fork_command_returns_new_thread_and_sends_cutoff_params() {
    let server = MockServer::start();
    let forked = run_json(
        &server,
        &[
            "fork",
            "--server",
            "work",
            "--json",
            "--last-turn",
            "turn_2",
            "--model",
            "gpt-5.6",
            "--effort",
            "ultra",
            "--service-tier",
            "priority",
            "--name",
            "Forked thread",
            "thread_1",
        ],
    );
    assert_eq!(forked["threadId"], "thread_fork");
    assert_eq!(forked["forkedFromThreadId"], "thread_1");
    assert_eq!(forked["lastTurnId"], "turn_2");
    assert_eq!(forked["model"], "gpt-5.6");
    assert_eq!(forked["effort"], "ultra");
    assert_eq!(forked["serviceTier"], "priority");

    let fork_params = server.params_for("thread/fork");
    assert_eq!(fork_params.len(), 1);
    assert_eq!(fork_params[0]["threadId"], "thread_1");
    assert_eq!(fork_params[0]["lastTurnId"], "turn_2");
    assert_eq!(fork_params[0]["excludeTurns"], true);
    assert_eq!(fork_params[0]["model"], "gpt-5.6");
    assert_eq!(fork_params[0]["config"]["model_reasoning_effort"], "ultra");
    assert_eq!(fork_params[0]["serviceTier"], "priority");
    assert_thread_yolo_params(&fork_params[0]);

    let name_params = server.params_for("thread/name/set");
    assert_eq!(name_params.len(), 1);
    assert_eq!(name_params[0]["threadId"], "thread_fork");
    assert_eq!(name_params[0]["name"], "Forked thread");
}

#[test]
fn custom_reasoning_effort_passes_through_to_app_server() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let created = run_json(
        &server,
        &[
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--effort",
            "provider-private-effort",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "provider-private-effort"
    );
}

#[test]
fn config_model_defaults_apply_to_new_threads_not_send_or_fork() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    write_config(
        &server,
        format!(
            r#"model = "gpt-5.5"
model_reasoning_effort = "high"

[servers.work]
type = "uds"
path = "{}"
"#,
            server.socket.display()
        ),
    );

    let completed = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--json", "say done",
        ],
    );
    assert_eq!(completed["threadId"], "thread_new");

    let accepted = run_json(
        &server,
        &[
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["threadId"], "thread_1");

    let forked = run_json(&server, &["fork", "--server", "work", "--json", "thread_1"]);
    assert_eq!(forked["threadId"], "thread_fork");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 1);
    assert_eq!(thread_start_params[0]["model"], "gpt-5.5");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "high"
    );

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert!(turn_start_params[0].get("model").is_none());
    assert!(turn_start_params[0].get("effort").is_none());
    assert!(turn_start_params[1].get("model").is_none());
    assert!(turn_start_params[1].get("effort").is_none());

    let fork_params = server.params_for("thread/fork");
    assert_eq!(fork_params.len(), 1);
    assert_eq!(fork_params[0]["threadId"], "thread_1");
    assert_eq!(fork_params[0]["excludeTurns"], true);
    assert!(fork_params[0].get("lastTurnId").is_none());
    assert!(fork_params[0].get("model").is_none());
    assert!(fork_params[0].get("config").is_none());
}

#[test]
fn server_model_defaults_override_global_and_cli_overrides_config() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    write_config(
        &server,
        format!(
            r#"model = "gpt-global"
model_reasoning_effort = "low"

[servers.work]
type = "uds"
path = "{}"
model = "gpt-5.5"
model_reasoning_effort = "high"
"#,
            server.socket.display()
        ),
    );

    let created = run_json(
        &server,
        &["new", "--server", "work", "--cwd", &cwd, "--json"],
    );
    assert_eq!(created["threadId"], "thread_new");

    let created = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--model", "gpt-cli", "--effort", "medium",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 2);
    assert_eq!(thread_start_params[0]["model"], "gpt-5.5");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "high"
    );
    assert_eq!(thread_start_params[1]["model"], "gpt-cli");
    assert_eq!(
        thread_start_params[1]["config"]["model_reasoning_effort"],
        "medium"
    );
}

#[test]
fn no_yolo_uses_app_server_permission_defaults() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let created = run_json(
        &server,
        &[
            "--no-yolo",
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let accepted = run_json(
        &server,
        &[
            "--no-yolo",
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["threadId"], "thread_1");

    let settings = run_json(
        &server,
        &[
            "--no-yolo",
            "settings",
            "show",
            "--server",
            "work",
            "--json",
            "thread_1",
        ],
    );
    assert_eq!(settings["model"], "gpt-5.1-codex");

    for params in server.params_for("thread/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("turn/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("thread/resume") {
        assert_no_yolo_params(&params);
    }
}

#[test]
fn golden_json_output_shapes_are_stable() {
    let server = MockServer::start();

    assert_eq!(
        run_json(&server, &["list", "--server", "work", "--json"]),
        json!({
            "server": "work",
            "threads": [
                {
                    "id": "thread_1",
                    "name": "Mock Thread",
                    "preview": "Mock preview",
                    "cwd": "/tmp/mock-work",
                    "status": { "type": "idle" },
                    "createdAt": 1_700_000_000_i64,
                    "updatedAt": 1_700_000_100_i64,
                    "experimentalThreadField": { "retained": true }
                }
            ],
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["search", "threads", "--server", "work", "--json", "mock"],
        ),
        json!({
            "server": "work",
            "results": [
                {
                    "thread": {
                        "id": "thread_1",
                        "name": "Mock Thread",
                        "preview": "Mock preview",
                        "cwd": "/tmp/mock-work",
                        "status": { "type": "idle" },
                        "createdAt": 1_700_000_000_i64,
                        "updatedAt": 1_700_000_100_i64,
                        "experimentalThreadField": { "retained": true }
                    },
                    "score": 1.0
                }
            ],
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(&server, &["show", "--server", "work", "--json", "thread_1"]),
        json!({
            "server": "work",
            "thread": {
                "id": "thread_1",
                "name": "Mock Thread",
                "preview": "Mock preview",
                "cwd": "/tmp/mock-work",
                "status": { "type": "idle" },
                "createdAt": 1_700_000_000_i64,
                "updatedAt": 1_700_000_100_i64,
                "experimentalThreadField": { "retained": true }
            },
            "turns": {
                "data": [
                    {
                        "id": "turn_1",
                        "status": "completed",
                        "startedAt": 1_700_000_050_i64,
                        "completedAt": 1_700_000_060_i64,
                        "experimentalTurnField": "retained",
                        "items": [
                            {
                                "id": "item_user",
                                "type": "userMessage",
                                "content": [{ "type": "text", "text": "hello" }]
                            },
                            {
                                "id": "item_agent",
                                "type": "agentMessage",
                                "text": "done"
                            }
                        ]
                    }
                ],
                "nextCursor": Value::Null,
                "backwardsCursor": Value::Null
            }
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["messages", "--server", "work", "--json", "thread_1"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "messages": [
                {
                    "role": "user",
                    "text": "hello",
                    "turnId": "turn_1",
                    "itemId": "item_user",
                    "turnStartedAt": 1_700_000_050_i64,
                    "turnCompletedAt": 1_700_000_060_i64
                },
                {
                    "role": "assistant",
                    "text": "done",
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "turnStartedAt": 1_700_000_050_i64,
                    "turnCompletedAt": 1_700_000_060_i64
                }
            ],
            "truncated": false,
            "nextCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["status", "--server", "work", "--json", "thread_1"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "thread": {
                "id": "thread_1",
                "name": "Mock Thread",
                "preview": "Mock preview",
                "cwd": "/tmp/mock-work",
                "status": { "type": "idle" },
                "createdAt": 1_700_000_000_i64,
                "updatedAt": 1_700_000_100_i64,
                "experimentalThreadField": { "retained": true }
            },
            "activeTurnId": Value::Null,
            "truncated": false
        })
    );
}

#[test]
fn golden_send_json_output_shapes_are_stable() {
    let server = MockServer::start();

    assert_eq!(
        run_json(
            &server,
            &["send", "--server", "work", "--json", "thread_1", "continue"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "turnId": "turn_1",
            "status": "completed",
            "progress": [
                {
                    "type": "accepted",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "status": "accepted"
                },
                {
                    "type": "progress",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "delta": "done"
                },
                {
                    "type": "completed",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "status": "completed"
                }
            ],
            "assistantResponses": [{ "itemId": "item_agent", "text": "done" }],
            "finalAssistantText": "done"
        })
    );

    assert_eq!(
        run_ndjson(
            &server,
            &[
                "send", "--server", "work", "--json", "--stream", "thread_1", "continue",
            ]
        ),
        vec![
            json!({
                "type": "accepted",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "status": "accepted"
            }),
            json!({
                "type": "progress",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "itemId": "item_agent",
                "delta": "done"
            }),
            json!({
                "type": "completed",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "status": "completed"
            }),
        ]
    );
}

#[test]
fn send_streams_ndjson_when_requested() {
    let server = MockServer::start();
    let output = server
        .command()
        .args([
            "send", "--server", "work", "--json", "--stream", "thread_1", "continue",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = String::from_utf8(output).expect("utf8");
    let events = lines
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ndjson"))
        .collect::<Vec<_>>();
    assert_eq!(events[0]["type"], "accepted");
    assert_eq!(events[1]["delta"], "done");
    assert_eq!(events.last().unwrap()["status"], "completed");
}

#[test]
fn send_human_stream_does_not_duplicate_completed_agent_message() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["send", "--server", "work", "thread_1", "continue"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert_eq!(text.matches("done").count(), 1);
    assert!(text.contains("done\nstatus"));
    assert!(text.contains("completed"));
}

#[test]
fn models_human_output_uses_model_fields() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["models", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("MODEL"));
    assert!(text.contains("NAME"));
    assert!(text.contains("gpt-5.5"));
    assert!(text.contains("GPT-5.5"));
    assert!(!text.starts_with("0"));
}

#[test]
fn send_falls_back_to_polling_when_turn_notifications_are_absent() {
    let server = MockServer::start_without_turn_notifications();
    let completed = run_json(
        &server,
        &["send", "--server", "work", "--json", "thread_1", "continue"],
    );
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finalAssistantText"], "done");
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "poll"
    );

    let output = server
        .command()
        .args(["send", "--server", "work", "thread_1", "continue"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert_eq!(text.match_indices("done").count(), 1, "{text}");
}

#[test]
fn wait_attaches_to_an_existing_turn_and_returns_its_terminal_result() {
    let server = MockServer::start_without_turn_notifications();
    let completed = run_json(
        &server,
        &[
            "wait",
            "--server",
            "work",
            "--json",
            "--timeout",
            "10",
            "thread_1",
            "turn_1",
        ],
    );

    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["threadId"], "thread_1");
    assert_eq!(completed["turnId"], "turn_1");
    assert_eq!(completed["finalAssistantText"], "done");
    assert!(server.methods().contains(&"thread/resume".to_string()));
    assert!(!server.methods().contains(&"thread/turns/list".to_string()));
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "snapshot"
    );
    let snapshot = completed["progress"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "assistantMessage")
        .expect("retained snapshot assistant event");
    assert_eq!(snapshot["source"], "snapshot");
    assert_eq!(snapshot["text"], "done");
}

#[test]
fn wait_completes_from_the_resume_snapshot_without_waiting_for_the_quiet_poll() {
    let server = MockServer::start_without_turn_notifications();
    let output = server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "300")
        .args([
            "wait",
            "--server",
            "work",
            "--json",
            "--timeout",
            "1",
            "thread_1",
            "turn_1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let completed: Value = serde_json::from_slice(&output).expect("terminal json");

    assert_eq!(completed["status"], "completed");
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "snapshot"
    );
    assert!(!server.methods().contains(&"thread/turns/list".to_string()));
}

#[test]
fn wait_timeout_covers_the_initial_resume_request() {
    let server = MockServer::start_without_turn_notifications();
    let started = std::time::Instant::now();
    server
        .command()
        .args([
            "wait",
            "--server",
            "work",
            "--timeout",
            "1",
            "thread_hung_resume",
            "turn_1",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "timed out waiting for turn `turn_1`",
        ));
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

#[test]
fn wait_timeout_cancels_a_hung_fallback_poll() {
    let server = MockServer::start_without_turn_notifications();
    let started = std::time::Instant::now();
    server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args([
            "wait",
            "--server",
            "work",
            "--timeout",
            "2",
            "thread_hung_poll",
            "turn_1",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "timed out waiting for turn `turn_1`",
        ));

    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn wait_and_events_follow_report_local_interrupts_while_a_poll_is_hung() {
    let server = MockServer::start_without_turn_notifications();
    for args in [
        vec![
            "wait",
            "--server",
            "work",
            "--timeout",
            "60",
            "thread_hung_poll",
            "turn_1",
        ],
        vec![
            "events",
            "follow",
            "--server",
            "work",
            "--timeout",
            "60",
            "thread_hung_poll",
            "turn_1",
        ],
    ] {
        let output = run_and_interrupt(&server, &args);
        assert_eq!(output.status.code(), Some(130));
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        assert!(stderr.contains("interrupted locally; turn is still running"));
        assert!(stderr.contains("threadId"));
        assert!(stderr.contains("thread_hung_poll"));
        assert!(stderr.contains("turnId"));
        assert!(stderr.contains("turn_1"));
    }
}

#[test]
fn wait_reports_local_interrupt_while_resume_is_hung() {
    let server = MockServer::start_without_turn_notifications();
    let output = run_and_interrupt(
        &server,
        &[
            "wait",
            "--server",
            "work",
            "--timeout",
            "60",
            "thread_hung_resume",
            "turn_1",
        ],
    );
    assert_eq!(output.status.code(), Some(130));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("interrupted locally; turn is still running"));
    assert!(stderr.contains("thread_hung_resume"));
    assert!(stderr.contains("turn_1"));
}

#[test]
fn result_reads_a_persisted_turn_without_starting_or_resuming_it() {
    let server = MockServer::start();
    let result = run_json(
        &server,
        &["result", "--server", "work", "--json", "thread_1", "turn_1"],
    );

    assert_eq!(result["status"], "completed");
    assert_eq!(result["threadId"], "thread_1");
    assert_eq!(result["turnId"], "turn_1");
    assert_eq!(result["finalAssistantText"], "done");
    assert_eq!(result["turn"]["experimentalTurnField"], "retained");
    assert_eq!(
        server.methods(),
        vec!["initialize", "initialized", "thread/turns/list"]
    );
}

#[test]
fn result_pages_until_it_finds_a_turn_beyond_the_server_page_cap() {
    let server = MockServer::start();
    let result = run_json(
        &server,
        &[
            "result",
            "--server",
            "work",
            "--json",
            "thread_result_paged",
            "turn_target_101",
        ],
    );

    assert_eq!(result["turnId"], "turn_target_101");
    assert_eq!(result["finalAssistantText"], "done");
    let params = server.params_for("thread/turns/list");
    assert_eq!(params.len(), 2);
    assert_eq!(params[0]["limit"], 100);
    assert!(params[0]["cursor"].is_null());
    assert_eq!(params[1]["limit"], 100);
    assert_eq!(params[1]["cursor"], "turn-page-2");
}

#[test]
fn result_rejects_an_empty_page_that_claims_another_cursor() {
    let server = MockServer::start();
    server
        .command()
        .args([
            "result",
            "--server",
            "work",
            "--json",
            "--max-turns",
            "1",
            "thread_result_empty_page",
            "turn_missing",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "empty turn page with a next cursor",
        ));
    assert_eq!(server.params_for("thread/turns/list").len(), 1);
}

#[test]
fn result_rejects_missing_or_non_string_turn_status() {
    let server = MockServer::start();
    for thread_id in ["thread_missing_status", "thread_invalid_status"] {
        server
            .command()
            .args([
                "result", "--server", "work", "--json", thread_id, "turn_bad",
            ])
            .assert()
            .code(3)
            .stderr(predicates::str::contains(
                "app-server returned a turn without a string status",
            ));
    }
}

#[test]
fn wait_rejects_a_persisted_turn_without_a_string_status() {
    let server = MockServer::start_without_turn_notifications();
    server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args([
            "wait",
            "--server",
            "work",
            "--timeout",
            "10",
            "thread_missing_status",
            "turn_bad",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "app-server returned a turn without a string status",
        ));
}

#[test]
fn events_follow_emits_ndjson_through_the_terminal_event() {
    let server = MockServer::start_without_turn_notifications();
    let events = server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args([
            "events",
            "follow",
            "--server",
            "work",
            "--timeout",
            "10",
            "thread_1",
            "turn_1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = String::from_utf8(events)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ndjson"))
        .collect::<Vec<_>>();

    assert_eq!(events.first().unwrap()["type"], "attached");
    assert!(events.first().unwrap().get("thread").is_none());
    assert_eq!(events.last().unwrap()["type"], "completed");
    let snapshot = events
        .iter()
        .find(|event| event["type"] == "assistantMessage")
        .expect("snapshot assistant message");
    assert_eq!(snapshot["source"], "snapshot");
    assert_eq!(snapshot["text"], "done");
    assert_eq!(events.last().unwrap()["source"], "snapshot");
    assert!(!server.methods().contains(&"thread/turns/list".to_string()));
}

#[test]
fn attachment_does_not_duplicate_snapshot_content_under_a_live_item_id() {
    let server = MockServer::start_without_turn_notifications();
    let output = run_json(
        &server,
        &[
            "wait",
            "--server",
            "work",
            "--json",
            "thread_snapshot_replay",
            "turn_1",
        ],
    );
    assert_eq!(output["finalAssistantText"], "done");
    assert_eq!(output["assistantResponses"].as_array().unwrap().len(), 1);
}

#[test]
fn terminal_notification_received_before_an_unmaterialized_error_is_not_lost() {
    let server = MockServer::start_without_turn_notifications();
    let output = run_json(
        &server,
        &[
            "send",
            "--server",
            "work",
            "--json",
            "thread_unmaterialized_terminal",
            "continue",
        ],
    );
    assert_eq!(output["status"], "completed");
}

#[test]
fn wait_rejects_turn_history_without_a_data_array() {
    let server = MockServer::start_without_turn_notifications();
    server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args([
            "wait",
            "--server",
            "work",
            "--timeout",
            "5",
            "thread_missing_turn_data",
            "turn_1",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/turns/list response missing data array",
        ));
}

#[test]
fn inject_forwards_validated_raw_response_items_without_starting_a_turn() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "inject",
            "--server",
            "work",
            "--json",
            "--items-json",
            r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"remember this"}]}]"#,
            "thread_1",
        ],
    );

    assert_eq!(output["status"], "accepted");
    assert_eq!(output["itemCount"], 1);
    let params = server.params_for("thread/inject_items");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["threadId"], "thread_1");
    assert_eq!(params[0]["items"][0]["role"], "user");
    assert!(!server.methods().contains(&"turn/start".to_string()));
}

#[test]
fn inject_rejects_a_non_object_success_result() {
    let server = MockServer::start();
    server
        .command()
        .args([
            "inject",
            "--server",
            "work",
            "--json",
            "--items-json",
            r#"[{"type":"message","role":"user","content":[]}]"#,
            "thread_invalid_inject_result",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/inject_items response must be an object",
        ));
}

#[test]
fn inject_reads_items_from_stdin_when_items_file_is_dash() {
    let server = MockServer::start();
    let output = server
        .command()
        .args([
            "inject",
            "--server",
            "work",
            "--json",
            "--items-file",
            "-",
            "thread_1",
        ])
        .write_stdin(r#"[{"type":"message","role":"assistant","content":[]}]"#)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&output).expect("json output");

    assert_eq!(output["itemCount"], 1);
    assert_eq!(
        server.params_for("thread/inject_items")[0]["items"][0]["role"],
        "assistant"
    );
}

#[test]
fn inject_rejects_non_array_and_empty_items_before_rpc_submission() {
    let server = MockServer::start();
    server
        .command()
        .args([
            "inject",
            "--server",
            "work",
            "--items-json",
            r#"{"type":"message"}"#,
            "thread_1",
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("must be a non-empty JSON array"));
    server
        .command()
        .args([
            "inject",
            "--server",
            "work",
            "--items-json",
            "[]",
            "thread_1",
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("must be a non-empty JSON array"));

    assert!(server.params_for("thread/inject_items").is_empty());
}

#[test]
fn inject_rejects_oversized_files_before_rpc_submission() {
    let server = MockServer::start();
    let temp = TempDir::new().expect("tempdir");
    let items = temp.path().join("oversized-items.json");
    fs::write(&items, vec![b' '; 16 * 1024 * 1024 + 1]).expect("write oversized input");

    server
        .command()
        .args([
            "inject",
            "--server",
            "work",
            "--items-file",
            items.to_str().expect("utf8 path"),
            "thread_1",
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("16777216-byte limit"));

    assert!(server.params_for("thread/inject_items").is_empty());
}

#[test]
fn send_retries_when_turn_history_is_not_materialized_yet() {
    let server = MockServer::start_with_unmaterialized_first_poll();
    let output = server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args(["send", "--server", "work", "--json", "thread_1", "continue"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let completed: Value = serde_json::from_slice(&output).expect("json output");

    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finalAssistantText"], "done");
    assert!(
        server
            .methods()
            .iter()
            .filter(|method| method.as_str() == "thread/turns/list")
            .count()
            >= 2
    );
}

#[test]
fn send_propagates_other_invalid_turn_history_errors() {
    let server = MockServer::start_rejecting_first_turns_list_with(
        -32600,
        "thread/turns/list rejected the requested items view",
    );
    server
        .command()
        .env("CODEX_TAMER_TURN_POLL_QUIET_SECS", "1")
        .args([
            "send", "--server", "work", "--json", "thread_1", "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "app-server `thread/turns/list` error -32600: thread/turns/list rejected the requested items view",
        ));

    assert_eq!(
        server
            .methods()
            .iter()
            .filter(|method| method.as_str() == "thread/turns/list")
            .count(),
        1
    );
}

#[test]
fn send_resumes_not_loaded_thread_before_retrying_turn_start() {
    let server = MockServer::start_requiring_resume_for_send();
    let accepted = run_json(
        &server,
        &[
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["threadId"], "thread_1");

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(retry_methods, ["turn/start", "thread/resume", "turn/start"]);

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert_turn_yolo_params(&turn_start_params[0]);
    assert_turn_yolo_params(&turn_start_params[1]);

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_thread_yolo_params(&thread_resume_params[0]);
}

#[test]
fn direct_input_capability_blocks_send_and_steer_before_submission() {
    let server = MockServer::start();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--no-wait",
            "thread_read_only",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_read_only` does not accept direct input",
        ));
    server
        .command()
        .args([
            "steer",
            "--server",
            "work",
            "thread_read_only",
            "turn_1",
            "adjust",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_read_only` does not accept direct input",
        ));
    assert!(server.params_for("turn/start").is_empty());
    assert!(server.params_for("turn/steer").is_empty());
}

#[test]
fn direct_input_capability_is_rechecked_after_resume() {
    let server = MockServer::start_requiring_resume_for_send();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--no-wait",
            "thread_denied_after_resume",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_denied_after_resume` does not accept direct input",
        ));
    assert_eq!(server.params_for("turn/start").len(), 1);
    assert_eq!(server.params_for("thread/resume").len(), 1);
}

#[test]
fn no_yolo_resume_retry_uses_app_server_permission_defaults() {
    let server = MockServer::start_requiring_resume_for_send();
    let accepted = run_json(
        &server,
        &[
            "--no-yolo",
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["status"], "accepted");

    for params in server.params_for("turn/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("thread/resume") {
        assert_no_yolo_params(&params);
    }
}

#[test]
fn settings_set_resumes_not_loaded_thread_before_retrying_update() {
    let server = MockServer::start_requiring_resume_for_settings_set();
    let updated = run_json(
        &server,
        &[
            "settings", "set", "--server", "work", "--json", "thread_1", "--effort", "high",
        ],
    );
    assert_eq!(updated["status"], "accepted");

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "thread/settings/update" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        retry_methods,
        [
            "thread/settings/update",
            "thread/resume",
            "thread/settings/update"
        ]
    );

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_thread_yolo_params(&thread_resume_params[0]);
}

#[test]
fn no_yolo_settings_set_resume_uses_app_server_permission_defaults() {
    let server = MockServer::start_requiring_resume_for_settings_set();
    let updated = run_json(
        &server,
        &[
            "--no-yolo",
            "settings",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "--effort",
            "high",
        ],
    );
    assert_eq!(updated["status"], "accepted");

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_no_yolo_params(&thread_resume_params[0]);
}

#[test]
fn resume_retry_requires_exact_thread_not_found_error_contract() {
    let server = MockServer::start_rejecting_turn_start_with(-32600, "missing thread: thread_1");
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ])
        .assert()
        .code(3);

    let methods = server.methods();
    assert_eq!(
        methods
            .iter()
            .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["turn/start"]
    );
}

#[test]
fn resume_retry_requires_invalid_request_error_code() {
    let server = MockServer::start_rejecting_turn_start_with(-32603, "thread not found: thread_1");
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ])
        .assert()
        .code(3);

    let methods = server.methods();
    assert_eq!(
        methods
            .iter()
            .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["turn/start"]
    );
}

#[test]
fn send_ignores_completion_for_a_different_turn_on_the_same_thread() {
    let server = MockServer::start_with_wrong_turn_completion();
    let completed = run_json(
        &server,
        &["send", "--server", "work", "--json", "thread_1", "continue"],
    );
    assert_eq!(completed["turnId"], "turn_1");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finalAssistantText"], "done");
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "poll"
    );
}

#[test]
fn failed_turn_exits_one_and_returns_terminal_json() {
    let server = MockServer::start_with_failed_turn();
    let output = server
        .command()
        .args(["send", "--server", "work", "--json", "thread_1", "continue"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let failed: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(failed["turnId"], "turn_1");
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["finalAssistantText"], "failed");
    assert_eq!(failed["error"]["code"], "mock_failure");
    assert_eq!(
        failed["progress"].as_array().unwrap().last().unwrap()["error"]["message"],
        "mock turn failed"
    );
}

#[test]
fn unknown_turn_status_notification_is_app_server_error() {
    let server = MockServer::start_with_unknown_turn_status();
    server
        .command()
        .args(["send", "--server", "work", "--json", "thread_1", "continue"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "app-server returned unrecognized turn status `mystery`",
        ));
}

#[test]
fn malformed_app_server_turn_start_is_exit_code_three() {
    let server = MockServer::start_with_malformed_turn_start();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "turn/start response missing turn.id",
        ));
}

#[test]
fn malformed_control_acknowledgements_are_exit_code_three() {
    let server = MockServer::start();

    server
        .command()
        .args([
            "steer",
            "--server",
            "work",
            "--json",
            "thread_invalid_steer_result",
            "turn_1",
            "adjust",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "turn/steer response missing turnId",
        ));

    server
        .command()
        .args([
            "interrupt",
            "--server",
            "work",
            "--json",
            "thread_invalid_interrupt_result",
            "turn_1",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "turn/interrupt response must be an object",
        ));
}

#[test]
fn mutation_commands_reject_non_object_app_server_results() {
    let cases = [
        (
            "thread/settings/update",
            vec![
                "settings", "set", "--server", "work", "thread_1", "--effort", "high",
            ],
        ),
        (
            "thread/name/set",
            vec!["name", "--server", "work", "thread_1", "New name"],
        ),
        (
            "thread/archive",
            vec!["archive", "--server", "work", "thread_1"],
        ),
        (
            "thread/unarchive",
            vec!["unarchive", "--server", "work", "thread_1"],
        ),
        (
            "thread/metadata/update",
            vec!["pin", "--server", "work", "thread_1"],
        ),
        (
            "thread/goal/set",
            vec![
                "goal",
                "set",
                "--server",
                "work",
                "thread_1",
                "--objective",
                "Ship",
            ],
        ),
        (
            "thread/goal/clear",
            vec!["goal", "clear", "--server", "work", "thread_1"],
        ),
    ];

    for (method, args) in cases {
        let server = MockServer::start_with_response(method, Value::Null);
        server
            .command()
            .args(args)
            .assert()
            .code(3)
            .stderr(predicates::str::contains(format!(
                "{method} response must be an object"
            )));
    }
}

#[test]
fn thread_mutations_reject_mismatched_acknowledgements() {
    let server = MockServer::start_with_response(
        "thread/metadata/update",
        json!({"thread": sample_pinned_thread("thread_other")}),
    );
    server
        .command()
        .args(["pin", "--server", "work", "thread_1"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/metadata/update response thread.id `thread_other` does not match `thread_1`",
        ));

    let server = MockServer::start_with_response(
        "thread/metadata/update",
        json!({"thread": sample_pinned_thread("thread_1")}),
    );
    server
        .command()
        .args(["unpin", "--server", "work", "thread_1"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/metadata/update response thread.isPinned does not match false",
        ));

    let server = MockServer::start_with_response(
        "thread/unarchive",
        json!({"thread": sample_thread("thread_other")}),
    );
    server
        .command()
        .args(["unarchive", "--server", "work", "thread_1"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/unarchive response thread.id `thread_other` does not match `thread_1`",
        ));
}

#[test]
fn goal_mutations_reject_malformed_or_mismatched_acknowledgements() {
    let server = MockServer::start_with_response(
        "thread/goal/set",
        json!({
            "goal": {
                "threadId": "thread_other",
                "objective": "Ship",
                "status": "active",
                "tokenBudget": 1000,
                "tokensUsed": 0,
                "timeUsedSeconds": 0,
                "createdAt": 1,
                "updatedAt": 1
            }
        }),
    );
    server
        .command()
        .args([
            "goal",
            "set",
            "--server",
            "work",
            "thread_1",
            "--objective",
            "Ship",
            "--token-budget",
            "1000",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/goal/set response goal.threadId `thread_other` does not match `thread_1`",
        ));

    let server = MockServer::start_with_response("thread/goal/clear", json!({"cleared": "yes"}));
    server
        .command()
        .args(["goal", "clear", "--server", "work", "thread_1"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/goal/clear response cleared must be a boolean",
        ));
}

#[test]
fn usage_redeem_rejects_unknown_consume_outcomes() {
    let server = MockServer::start_with_response(
        "account/rateLimitResetCredit/consume",
        json!({"outcome": "maybe"}),
    );
    server.allow_rate_limit_reset();

    server
        .command()
        .args(["usage", "redeem", "--server", "work", "--json"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "account/rateLimitResetCredit/consume response has unknown outcome `maybe`",
        ));
}

#[test]
fn steer_rejects_an_acknowledgement_for_a_different_turn() {
    let server = MockServer::start();

    server
        .command()
        .args([
            "steer",
            "--server",
            "work",
            "--json",
            "thread_mismatched_steer_result",
            "turn_1",
            "adjust",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("turn/steer response"));
}

#[test]
fn result_rejects_non_string_next_cursor() {
    let server = MockServer::start();

    server
        .command()
        .args([
            "result",
            "--server",
            "work",
            "--json",
            "thread_invalid_next_cursor",
            "turn_missing",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread/turns/list response nextCursor must be a string or null",
        ));
}

#[test]
fn control_and_goal_commands_return_acknowledgements() {
    let server = MockServer::start();

    assert_eq!(
        run_json(
            &server,
            &[
                "steer", "--server", "work", "--json", "thread_1", "turn_1", "adjust"
            ]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "turnId": "turn_1",
            "status": "accepted"
        })
    );
    assert_eq!(
        run_json(
            &server,
            &[
                "interrupt",
                "--server",
                "work",
                "--json",
                "thread_1",
                "turn_1"
            ]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "turnId": "turn_1",
            "status": "accepted"
        })
    );
    assert_eq!(
        run_json(
            &server,
            &["name", "--server", "work", "--json", "thread_1", "New name"]
        )["name"],
        "New name"
    );
    let pinned = run_json(&server, &["pin", "--server", "work", "--json", "thread_1"]);
    assert_eq!(pinned["pinned"], true);
    assert_eq!(pinned["thread"]["isPinned"], true);
    let unpinned = run_json(
        &server,
        &["unpin", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(unpinned["pinned"], false);
    assert_eq!(unpinned["thread"]["isPinned"], false);
    assert_eq!(
        run_json(
            &server,
            &["archive", "--server", "work", "--json", "thread_1"]
        )["archived"],
        true
    );
    let unarchived = run_json(
        &server,
        &["unarchive", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(unarchived["archived"], false);
    assert_eq!(unarchived["thread"]["id"], "thread_1");
    let metadata_params = server.params_for("thread/metadata/update");
    assert_eq!(
        metadata_params[0],
        json!({"threadId": "thread_1", "isPinned": true})
    );
    assert_eq!(
        metadata_params[1],
        json!({"threadId": "thread_1", "isPinned": false})
    );
    assert_eq!(
        run_json(
            &server,
            &["goal", "get", "--server", "work", "--json", "thread_1"]
        )["goal"]["status"],
        "active"
    );
    let goal_set = run_json(
        &server,
        &[
            "goal",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "--objective",
            "Ship",
            "--status",
            "active",
            "--token-budget",
            "1000",
        ],
    );
    assert_eq!(goal_set["goal"]["objective"], "Ship");
    assert_eq!(goal_set["goal"]["tokenBudget"].as_i64().unwrap(), 1000);
    let goal_get = run_json(
        &server,
        &["goal", "get", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(goal_get["goal"]["tokenBudget"].as_i64().unwrap(), 1000);
    assert_eq!(
        run_json(
            &server,
            &["goal", "clear", "--server", "work", "--json", "thread_1"]
        )["cleared"],
        true
    );

    let methods = server.methods();
    assert!(methods.iter().any(|method| method == "turn/steer"));
    assert!(methods.iter().any(|method| method == "thread/goal/clear"));
}

#[test]
fn steer_never_resumes_a_thread_that_is_not_loaded_on_the_target_runtime() {
    let server = MockServer::start_requiring_resume_for_steer();
    server
        .command()
        .args([
            "steer", "--server", "work", "--json", "thread_1", "turn_1", "adjust",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("thread not found"));

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "turn/steer" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(retry_methods, ["turn/steer"]);
    assert!(server.params_for("thread/resume").is_empty());
}

#[test]
fn invalid_new_prompt_flags_fail_before_connecting() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    server
        .command()
        .args([
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--json",
            "--no-wait",
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "new without PROMPT cannot use --no-wait",
        ));
}
