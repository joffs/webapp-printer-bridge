//! HTTP + WebSocket server.
//!
//! * `ws://<bind>:<port>/printer[?token=...]` - print requests, results are broadcast to
//!   every connected `/printer` client (same as the Java bridge).
//! * `GET /system/printers.json`, `GET /system/version.json` - read-only info.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc, watch};
use tower_http::cors::CorsLayer;

use crate::bridge::{self, AppState};
use crate::protocol::{PrintServiceDto, VersionDto};
use crate::{APP_ID, APP_NAME, VERSION};

#[derive(Clone)]
struct Ctx {
    state: Arc<AppState>,
    results: broadcast::Sender<String>,
    shutdown: watch::Receiver<bool>,
}

struct Running {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// Owns the async runtime and the currently running server instance.
pub struct ServerController {
    rt: Runtime,
    state: Arc<AppState>,
    running: Mutex<Option<Running>>,
}

impl ServerController {
    pub fn new(state: Arc<AppState>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            rt,
            state,
            running: Mutex::new(None),
        })
    }

    /// Bind and start serving with the current config. Bind errors are returned
    /// (e.g. the configured address does not exist on this machine).
    pub fn start(&self) -> Result<SocketAddr> {
        let config = self.state.config();
        let (bind, port) = (config.server.bind.clone(), config.server.port);
        let listener = self
            .rt
            .block_on(tokio::net::TcpListener::bind((bind.as_str(), port)))
            .with_context(|| format!("Cannot listen on {bind}:{port}"))?;
        let addr = listener.local_addr()?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (results, _) = broadcast::channel(256);
        let ctx = Ctx {
            state: self.state.clone(),
            results,
            shutdown: shutdown_rx.clone(),
        };
        let app = router(ctx, config.printer.enabled);

        let mut stop = shutdown_rx;
        let task = self.rt.spawn(async move {
            let served = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stop.changed().await;
            })
            .await;
            if let Err(e) = served {
                tracing::error!("Server error: {e}");
            }
        });

        *self.running.lock().unwrap() = Some(Running {
            shutdown: shutdown_tx,
            task,
        });
        tracing::info!("{APP_NAME} {VERSION} running on {addr}");
        Ok(addr)
    }

    pub fn stop(&self) {
        let Some(running) = self.running.lock().unwrap().take() else {
            return;
        };
        tracing::info!("Stopping server");
        let _ = running.shutdown.send(true);
        let _ = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), running.task).await });
    }
}

fn router(ctx: Ctx, printer_enabled: bool) -> Router {
    let mut app = Router::new()
        .route("/system/printers.json", get(printers))
        .route("/system/version.json", get(version))
        .route_layer(middleware::from_fn_with_state(ctx.clone(), http_auth));
    if printer_enabled {
        app = app.route("/printer", get(printer_socket));
    }
    app.layer(CorsLayer::permissive()).with_state(ctx)
}

/// HTTP token check (Java parity): `Authorization` header ending with the token
/// (`Bearer <token>`), or HTTP Basic auth with the token as password.
async fn http_auth(State(ctx): State<Ctx>, request: Request, next: Next) -> Response {
    let config = ctx.state.config();
    let auth = &config.server.authentication;
    if !auth.enabled {
        return next.run(request).await;
    }

    if let Some(token) = auth.token.as_deref() {
        let header = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let basic_password = header
            .strip_prefix("Basic ")
            .and_then(|b64| {
                base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .ok()
            })
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|creds| creds.split_once(':').map(|(_, pass)| pass.to_string()));
        if header.ends_with(token) || basic_password.as_deref() == Some(token) {
            return next.run(request).await;
        }
    }

    let mut response = (StatusCode::UNAUTHORIZED, "Token mismatch").into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Token required\""),
    );
    response
}

async fn printers() -> Response {
    match tokio::task::spawn_blocking(crate::printing::list_printers).await {
        Ok(Ok(names)) => Json(
            names
                .into_iter()
                .map(|name| PrintServiceDto {
                    name,
                    description: String::new(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn version() -> Json<VersionDto> {
    Json(VersionDto {
        app_name: APP_NAME,
        app_id: APP_ID,
        version: VERSION,
    })
}

async fn printer_socket(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(ctx): State<Ctx>,
) -> Response {
    let config = ctx.state.config();
    let auth = &config.server.authentication;
    let authorized = !auth.enabled
        || auth.token.as_deref() == Some(query.get("token").map_or("", String::as_str));

    ws.max_message_size(usize::MAX)
        .max_frame_size(usize::MAX)
        .on_upgrade(move |socket| connection(socket, ctx, peer, authorized))
}

async fn connection(socket: WebSocket, ctx: Ctx, peer: SocketAddr, authorized: bool) {
    let (mut sender, mut receiver) = socket.split();

    if !authorized {
        tracing::warn!("{peer} rejected: invalid token");
        let close = CloseFrame {
            code: 1003,
            reason: "Invalid token".into(),
        };
        let _ = sender.send(Message::Close(Some(close))).await;
        return;
    }
    tracing::info!("{peer} connected to /printer");

    // Jobs from one connection run in order; results go to all /printer clients.
    let (jobs, mut job_queue) = mpsc::unbounded_channel::<String>();
    let worker_ctx = ctx.clone();
    tokio::spawn(async move {
        while let Some(text) = job_queue.recv().await {
            if let Some(result) = bridge::handle_message(worker_ctx.state.clone(), text).await
                && let Ok(json) = serde_json::to_string(&result)
            {
                let _ = worker_ctx.results.send(json);
            }
        }
    });

    let mut results = ctx.results.subscribe();
    let mut shutdown = ctx.shutdown.clone();
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(5));
        ping.tick().await;
        loop {
            let message = tokio::select! {
                result = results.recv() => match result {
                    Ok(json) => Message::Text(json.into()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = ping.tick() => Message::Ping(Default::default()),
                _ = shutdown.changed() => {
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
            };
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let mut shutdown = ctx.shutdown.clone();
    let mut writer = writer;
    loop {
        let message = tokio::select! {
            message = receiver.next() => message,
            _ = shutdown.changed() => {
                // Server stopping: let the writer send its Close frame first.
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer).await;
                break;
            }
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                tracing::info!("{peer} sent message to /printer: {}", truncate(&text, 300));
                let _ = jobs.send(text.to_string());
            }
            Some(Ok(Message::Binary(_))) => {
                tracing::error!("PrinterWebSocketService: binary data not supported");
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
            Some(Ok(_)) => {}
        }
    }

    tracing::info!("{peer} disconnected from /printer");
    writer.abort();
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes)", &text[..end], text.len())
}
