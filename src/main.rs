use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Mutex,
};
use tracing::{error, info};

const MPV_SOCKET: &str = "/tmp/mpv-socket";
const UI_HTML: &str = include_str!("ui.html");

#[derive(Clone)]
struct AppState {
    mpv_socket: Arc<Mutex<Option<UnixStream>>>,
}

#[derive(Deserialize, Serialize, Debug)]
struct MpvCommand {
    command: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "action")]
enum ClientMessage {
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "play")]
    Play,
    #[serde(rename = "toggle")]
    Toggle,
    #[serde(rename = "seek")]
    Seek { seconds: f64 },
    #[serde(rename = "volume")]
    Volume { value: f64 },
    #[serde(rename = "subtitle_track")]
    SubtitleTrack { id: i64 },
    #[serde(rename = "subtitle_off")]
    SubtitleOff,
    #[serde(rename = "audio_track")]
    AudioTrack { id: i64 },
    #[serde(rename = "fullscreen")]
    Fullscreen,
    #[serde(rename = "get_status")]
    GetStatus,
}

async fn send_mpv_command(cmd: MpvCommand) -> Result<String, String> {
    let mut stream = UnixStream::connect(MPV_SOCKET)
        .await
        .map_err(|e| format!("Cannot connect to mpv socket: {}", e))?;

    let mut payload = serde_json::to_string(&cmd).map_err(|e| e.to_string())?;
    payload.push('\n');

    stream.write_all(payload.as_bytes()).await.map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

fn build_mpv_command(msg: ClientMessage) -> MpvCommand {
    use serde_json::json;
    let command = match msg {
        ClientMessage::Pause => vec![json!("set_property"), json!("pause"), json!(true)],
        ClientMessage::Play => vec![json!("set_property"), json!("pause"), json!(false)],
        ClientMessage::Toggle => vec![json!("cycle"), json!("pause")],
        ClientMessage::Seek { seconds } => vec![json!("seek"), json!(seconds), json!("relative")],
        ClientMessage::Volume { value } => vec![json!("set_property"), json!("volume"), json!(value)],
        ClientMessage::SubtitleTrack { id } => vec![json!("set_property"), json!("sid"), json!(id)],
        ClientMessage::SubtitleOff => vec![json!("set_property"), json!("sid"), json!("no")],
        ClientMessage::AudioTrack { id } => vec![json!("set_property"), json!("aid"), json!(id)],
        ClientMessage::Fullscreen => vec![json!("cycle"), json!("fullscreen")],
        ClientMessage::GetStatus => vec![json!("get_property"), json!("playback-time")],
    };
    MpvCommand { command }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, _state: AppState) {
    info!("WebSocket client connected");
    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => { error!("WebSocket error: {}", e); break; }
        };
        if let Message::Text(text) = msg {
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(client_msg) => {
                    let cmd = build_mpv_command(client_msg);
                    match send_mpv_command(cmd).await {
                        Ok(response) => { let _ = socket.send(Message::Text(response)).await; }
                        Err(e) => { let _ = socket.send(Message::Text(format!(r#"{{"error":"{}"}}"#, e))).await; }
                    }
                }
                Err(e) => { error!("Parse error: {}", e); }
            }
        }
    }
    info!("WebSocket client disconnected");
}

async fn index_handler() -> Html<&'static str> {
    Html(UI_HTML)
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let state = AppState { mpv_socket: Arc::new(Mutex::new(None)) };
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state);
    let addr = "0.0.0.0:3000";
    info!("mpv-remote listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
