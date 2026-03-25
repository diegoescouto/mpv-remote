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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
};
use tracing::{error, info};

const MPV_SOCKET: &str = "/tmp/mpv-socket";
const UI_HTML: &str = include_str!("ui.html");

#[derive(Clone)]
struct AppState {
    _phantom: Arc<Mutex<()>>,
}

#[derive(Deserialize, Serialize, Debug)]
struct MpvCommand {
    command: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
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
    #[serde(rename = "seek_abs")]
    SeekAbs { seconds: f64 },
    #[serde(rename = "volume")]
    Volume { value: f64 },
    #[serde(rename = "subtitle_track")]
    SubtitleTrack { id: serde_json::Value },
    #[serde(rename = "subtitle_off")]
    SubtitleOff,
    #[serde(rename = "audio_track")]
    AudioTrack { id: i64 },
    #[serde(rename = "fullscreen")]
    Fullscreen,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "get_status")]
    GetStatus,
    #[serde(rename = "get_tracks")]
    GetTracks,
    #[serde(rename = "get_metadata")]
    GetMetadata,
}

async fn send_mpv(cmd: MpvCommand) -> Result<String, String> {
    let stream = UnixStream::connect(MPV_SOCKET)
        .await
        .map_err(|e| format!("mpv not running: {}", e))?;

    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_string(&cmd).map_err(|e| e.to_string())?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await.map_err(|e| e.to_string())?;

    let mut lines = BufReader::new(reader).lines();
    // read first non-event line
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("\"error\"") || line.contains("\"data\"") {
            return Ok(line);
        }
    }
    Ok("{}".to_string())
}

async fn mpv_get(property: &str) -> serde_json::Value {
    let cmd = MpvCommand {
        command: vec![
            serde_json::json!("get_property"),
            serde_json::json!(property),
        ],
        request_id: None,
    };
    match send_mpv(cmd).await {
        Ok(r) => serde_json::from_str(&r).unwrap_or(serde_json::json!(null)),
        Err(_) => serde_json::json!(null),
    }
}

async fn get_full_status() -> serde_json::Value {
    let pos = mpv_get("playback-time").await;
    let duration = mpv_get("duration").await;
    let paused = mpv_get("pause").await;
    let volume = mpv_get("volume").await;
    let title = mpv_get("media-title").await;
    let aid = mpv_get("aid").await;
    let sid = mpv_get("sid").await;
    let track_list = mpv_get("track-list").await;

    serde_json::json!({
        "type": "status",
        "position": pos["data"],
        "duration": duration["data"],
        "paused": paused["data"],
        "volume": volume["data"],
        "title": title["data"],
        "aid": aid["data"],
        "sid": sid["data"],
        "track_list": track_list["data"],
    })
}

fn build_mpv_command(msg: ClientMessage) -> Option<MpvCommand> {
    use serde_json::json;
    let command = match msg {
        ClientMessage::Pause => vec![json!("set_property"), json!("pause"), json!(true)],
        ClientMessage::Play => vec![json!("set_property"), json!("pause"), json!(false)],
        ClientMessage::Toggle => vec![json!("cycle"), json!("pause")],
        ClientMessage::Seek { seconds } => vec![json!("seek"), json!(seconds), json!("relative")],
        ClientMessage::SeekAbs { seconds } => vec![json!("seek"), json!(seconds), json!("absolute")],
        ClientMessage::Volume { value } => vec![json!("set_property"), json!("volume"), json!(value)],
        ClientMessage::SubtitleTrack { id } => vec![json!("set_property"), json!("sid"), id],
        ClientMessage::SubtitleOff => vec![json!("set_property"), json!("sid"), json!("no")],
        ClientMessage::AudioTrack { id } => vec![json!("set_property"), json!("aid"), json!(id)],
        ClientMessage::Fullscreen => vec![json!("cycle"), json!("fullscreen")],
        ClientMessage::Stop => vec![json!("stop")],
        ClientMessage::GetStatus | ClientMessage::GetTracks | ClientMessage::GetMetadata => {
            return None;
        }
    };
    Some(MpvCommand { command, request_id: None })
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, _state: AppState) {
    info!("WebSocket client connected");

    // send initial status
    let status = get_full_status().await;
    let _ = socket.send(Message::Text(status.to_string())).await;

    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => { error!("WS error: {}", e); break; }
        };

        if let Message::Text(text) = msg {
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(client_msg) => {
                    match client_msg {
                        ClientMessage::GetStatus | ClientMessage::GetTracks | ClientMessage::GetMetadata => {
                            let status = get_full_status().await;
                            let _ = socket.send(Message::Text(status.to_string())).await;
                        }
                        other => {
                            if let Some(cmd) = build_mpv_command(other) {
                                match send_mpv(cmd).await {
                                    Ok(_) => {
                                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                        let status = get_full_status().await;
                                        let _ = socket.send(Message::Text(status.to_string())).await;
                                    }
                                    Err(e) => {
                                        let _ = socket.send(Message::Text(
                                            serde_json::json!({"type":"error","message": e}).to_string()
                                        )).await;
                                    }
                                }
                            }
                        }
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
    let state = AppState { _phantom: Arc::new(Mutex::new(())) };
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
