//! `herdr web` — xterm.js <-> herdr PTY bridge (feature = "web").
//!
//! gerchowl/herdr#131 — first-class, in-tree port of the #109 MVP. Architecture
//! (per the spike verdict, parent #109):
//!
//!   browser (xterm.js) <--WS--> herdr web <--PTY--> herdr client (terminal-ansi)
//!
//! Each WebSocket spawns a herdr **client** in a PTY with
//! `HERDR_RENDER_ENCODING=terminal-ansi`; herdr's server pre-diffs to ANSI and
//! the client is a stdout passthrough, so xterm.js writes the byte stream
//! straight to its buffer — no JS painting, no rerender. On an always-on host
//! (e.g. sage) the client attaches to the persistent `herdr server` daemon, so
//! the phone shares that node's live session AND its fleet gossip view.
//!
//! Security boundary (v1): this binds loopback only and is fronted by
//! `tailscale serve` (tailnet identity). Three guards back that up:
//!   1. refuse a non-loopback `--bind` unless `--allow-non-loopback`,
//!   2. refuse to start if `tailscale funnel` (PUBLIC) is active, unless
//!      `--allow-funnel`,
//!   3. same-origin check on the WS upgrade (CSWSH defence).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const DEFAULT_BIND: &str = "127.0.0.1:7681";

#[derive(RustEmbed)]
#[folder = "assets/web/"]
struct WebAssets;

/// Parsed `herdr web` configuration.
struct WebConfig {
    bind: SocketAddr,
    herdr_bin: PathBuf,
    herdr_args: Vec<String>,
    allow_non_loopback: bool,
    allow_funnel: bool,
    allowed_origins: Vec<String>,
    allow_any_origin: bool,
}

#[derive(Clone)]
struct AppState {
    herdr_bin: Arc<PathBuf>,
    herdr_args: Arc<Vec<String>>,
    allowed_origins: Arc<Vec<String>>,
    allow_any_origin: bool,
}

pub fn run_web_command(args: &[String]) -> std::io::Result<i32> {
    let cfg = match parse_args(args) {
        ParseResult::Ok(cfg) => cfg,
        ParseResult::Help => {
            print_web_help();
            return Ok(0);
        }
        ParseResult::Err(code) => return Ok(code),
    };

    // P0 #1 — loopback guard. The bridge is a full interactive shell; binding a
    // routable interface exposes it without the tailscale-serve auth boundary.
    if !cfg.bind.ip().is_loopback() && !cfg.allow_non_loopback {
        eprintln!(
            "herdr web: refusing to bind non-loopback address {}",
            cfg.bind
        );
        eprintln!("  the web bridge is a full shell; front it with `tailscale serve` on loopback.");
        eprintln!("  pass --allow-non-loopback only if you have another auth layer in front.");
        return Ok(2);
    }

    // P0 #2 — funnel guard. `tailscale funnel` publishes to the PUBLIC internet,
    // so a one-word slip from `serve` would expose a root shell. Refuse if we can
    // prove funnel is active, and only warn if tailscale state is unreadable (not
    // every host fronts with tailscale).
    if !cfg.allow_funnel {
        match tailscale_funnel_status() {
            FunnelCheck::Active => {
                eprintln!(
                    "herdr web: refusing to start — `tailscale funnel` is active on this node."
                );
                eprintln!(
                    "  funnel publishes to the PUBLIC internet; this bridge is a full shell."
                );
                eprintln!(
                    "  use `tailscale serve` (tailnet-only), or pass --allow-funnel to override."
                );
                return Ok(2);
            }
            FunnelCheck::Inactive => {}
            FunnelCheck::Unknown => {
                eprintln!(
                    "herdr web: could not verify tailscale funnel state; ensure you front this \
                     with `tailscale serve` (tailnet-only), NOT `tailscale funnel`."
                );
            }
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(cfg))
}

async fn serve(cfg: WebConfig) -> std::io::Result<i32> {
    let bind = cfg.bind;
    let state = AppState {
        herdr_bin: Arc::new(cfg.herdr_bin),
        herdr_args: Arc::new(cfg.herdr_args),
        allowed_origins: Arc::new(cfg.allowed_origins),
        allow_any_origin: cfg.allow_any_origin,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(static_handler)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("herdr web listening on http://{}/", bind);
    eprintln!("herdr web: listening on http://{}/", bind);
    axum::serve(listener, app).await?;
    Ok(0)
}

// ---- HTTP / static assets --------------------------------------------------

async fn static_handler(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };
    serve_asset(path)
}

fn serve_asset(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => (
            [(header::CONTENT_TYPE, content_type_for(path))],
            content.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

// ---- WebSocket -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    /// First message: terminal size. The bridge defers spawning herdr until
    /// this lands so the PTY is sized correctly from the start.
    Init { cols: u16, rows: u16 },
    /// xterm resized; pty.resize() on the master.
    Resize { cols: u16, rows: u16 },
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    // P0 #3 — CSWSH defence. A page the phone visits in its browser could open
    // a WebSocket to this endpoint and ride the tailnet auth transparently, so
    // reject upgrades whose Origin isn't same-origin or allow-listed.
    let origin = header_str(&headers, header::ORIGIN.as_str());
    let host = header_str(&headers, header::HOST.as_str());
    if !origin_allowed(
        origin.as_deref(),
        host.as_deref(),
        &state.allowed_origins,
        state.allow_any_origin,
    ) {
        warn!(?origin, ?host, "rejecting cross-origin WS upgrade");
        return (StatusCode::FORBIDDEN, "cross-origin websocket rejected").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    if let Err(e) = pump(socket, state).await {
        warn!(error = %e, "ws session ended with error");
    } else {
        info!("ws session ended cleanly");
    }
}

async fn pump(socket: WebSocket, state: AppState) -> anyhow::Result<()> {
    use anyhow::Context;
    let (mut ws_sink, mut ws_stream) = socket.split();

    // 1. Wait for the init message with cols/rows.
    let (cols, rows) = loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::Init { cols, rows }) => break (cols.max(1), rows.max(1)),
                // Tolerate resize-before-init from a racing client.
                Ok(ClientMsg::Resize { cols, rows }) => break (cols.max(1), rows.max(1)),
                Err(e) => {
                    warn!(error = %e, "ignoring non-init control msg pre-init");
                    continue;
                }
            },
            Some(Ok(Message::Binary(_))) => {
                warn!("ignoring pre-init binary frame");
                continue;
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return Ok(()),
            Some(Err(e)) => return Err(e.into()),
        }
    };

    info!(cols, rows, "ws init — spawning herdr in PTY");

    // 2. Spawn herdr in a PTY.
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(state.herdr_bin.as_os_str());
    for a in state.herdr_args.iter() {
        cmd.arg(a);
    }
    // Server-side ANSI diff encoding — the whole reason this bridge exists.
    cmd.env("HERDR_RENDER_ENCODING", "terminal-ansi");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLUMNS", cols.to_string());
    cmd.env("LINES", rows.to_string());
    // Kitty graphics is the known v1 cut — disable image cell reporting.
    cmd.env("HERDR_CELL_WIDTH_PX", "0");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair.slave.spawn_command(cmd).context("spawning herdr")?;
    drop(pair.slave);

    let master = pair.master;
    let reader = master.try_clone_reader().context("clone pty reader")?;
    let writer = master.take_writer().context("take pty writer")?;

    // PTY -> WS: blocking reader thread pushing chunks into a tokio channel.
    let (pty_tx, mut pty_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || pty_reader_loop(reader, pty_tx));

    // Resize channel: WS task -> resize listener.
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(8);

    // portable_pty's MasterPty is !Sync, so we guard it with a Mutex so the
    // resize task can borrow it.
    let master = Arc::new(std::sync::Mutex::new(master));

    // WS -> PTY task. Writes go through a blocking thread because
    // `Box<dyn Write + Send>` is sync.
    let (stdin_tx, stdin_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || pty_writer_loop(writer, stdin_rx));

    let ws_to_pty = tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(b)) => {
                    if stdin_tx.send(b).is_err() {
                        break;
                    }
                }
                Ok(Message::Text(t)) => {
                    // Disambiguate control messages (JSON) from raw keystrokes
                    // by attempting to parse first.
                    if let Ok(ctrl) = serde_json::from_str::<ClientMsg>(&t) {
                        match ctrl {
                            ClientMsg::Resize { cols, rows } => {
                                let _ = resize_tx.send((cols.max(1), rows.max(1))).await;
                            }
                            ClientMsg::Init { .. } => { /* ignore re-init */ }
                        }
                    } else if stdin_tx.send(t.into_bytes()).is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
            }
        }
        debug!("ws->pty loop ended");
    });

    // Resize listener.
    let master_for_resize = master.clone();
    let resize_task = tokio::spawn(async move {
        while let Some((cols, rows)) = resize_rx.recv().await {
            let guard = master_for_resize.lock().unwrap();
            if let Err(e) = guard.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                warn!(error = %e, "pty resize");
            } else {
                debug!(cols, rows, "pty resized");
            }
        }
    });

    // Child waiter.
    let (child_done_tx, mut child_done_rx) = mpsc::channel::<()>(1);
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = child_done_tx.blocking_send(());
    });

    // PTY -> WS forwarder.
    let pty_forwarder = async {
        while let Some(chunk) = pty_rx.recv().await {
            if ws_sink.send(Message::Binary(chunk)).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = pty_forwarder => { debug!("pty->ws drained"); }
        _ = ws_to_pty => { debug!("ws->pty task ended"); }
        _ = child_done_rx.recv() => { info!("herdr child exited"); }
    }

    // Dropping the master closes the PTY; herdr will see EOF and exit.
    drop(master);
    resize_task.abort();
    Ok(())
}

fn pty_reader_loop(mut reader: Box<dyn std::io::Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    use std::io::Read;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) => {
                debug!(error = %e, "pty read ended");
                break;
            }
        }
    }
}

fn pty_writer_loop(
    mut writer: Box<dyn std::io::Write + Send>,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
) {
    use std::io::Write;
    while let Ok(chunk) = rx.recv() {
        if writer.write_all(&chunk).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

// ---- Security helpers (pure, unit-tested) ----------------------------------

/// Same-origin / allow-list check for the WS upgrade (CSWSH defence).
///
/// - `allow_any` bypasses the check (explicit `--allow-any-origin`).
/// - An absent Origin is allowed: non-browser clients (curl, native) don't send
///   one, and CSWSH requires a browser that always does.
/// - Otherwise the Origin's authority (host[:port], scheme stripped) must equal
///   the request Host header (same-origin) or appear in the allow-list.
fn origin_allowed(
    origin: Option<&str>,
    host: Option<&str>,
    allowed: &[String],
    allow_any: bool,
) -> bool {
    if allow_any {
        return true;
    }
    let Some(origin) = origin else {
        return true;
    };
    let origin_authority = origin.split_once("://").map(|(_, a)| a).unwrap_or(origin);
    if allowed.iter().any(|a| a == origin || a == origin_authority) {
        return true;
    }
    matches!(host, Some(h) if h == origin_authority)
}

enum FunnelCheck {
    Active,
    Inactive,
    Unknown,
}

fn tailscale_funnel_status() -> FunnelCheck {
    let output = std::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                Ok(value) => {
                    if funnel_active(&value) {
                        FunnelCheck::Active
                    } else {
                        FunnelCheck::Inactive
                    }
                }
                Err(_) => FunnelCheck::Unknown,
            }
        }
        _ => FunnelCheck::Unknown,
    }
}

/// `tailscale serve status --json` reports funnel via an `AllowFunnel` map of
/// `host:port -> bool`. Any `true` value means funnel is live on this node.
fn funnel_active(status: &serde_json::Value) -> bool {
    status
        .get("AllowFunnel")
        .and_then(|v| v.as_object())
        .map(|m| m.values().any(|v| v.as_bool() == Some(true)))
        .unwrap_or(false)
}

// ---- Arg parsing -----------------------------------------------------------

enum ParseResult {
    Ok(WebConfig),
    Help,
    Err(i32),
}

fn default_herdr_bin() -> PathBuf {
    if let Ok(v) = std::env::var("HERDR_WEB_HERDR_BIN") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("herdr"))
}

fn parse_args(args: &[String]) -> ParseResult {
    let mut bind_raw = std::env::var("HERDR_WEB_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    if bind_raw.is_empty() {
        bind_raw = DEFAULT_BIND.to_string();
    }
    let mut herdr_bin = default_herdr_bin();
    let mut herdr_args: Vec<String> = Vec::new();
    let mut allow_non_loopback = false;
    let mut allow_funnel = false;
    let mut allow_any_origin = false;
    let mut allowed_origins: Vec<String> = std::env::var("HERDR_WEB_ALLOWED_ORIGINS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "help" | "--help" | "-h" => return ParseResult::Help,
            "--bind" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("herdr web: missing value for --bind");
                    return ParseResult::Err(2);
                };
                bind_raw = v.clone();
                i += 2;
            }
            "--herdr-bin" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("herdr web: missing value for --herdr-bin");
                    return ParseResult::Err(2);
                };
                herdr_bin = PathBuf::from(v);
                i += 2;
            }
            "--allowed-origin" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("herdr web: missing value for --allowed-origin");
                    return ParseResult::Err(2);
                };
                allowed_origins.push(v.clone());
                i += 2;
            }
            "--allow-non-loopback" => {
                allow_non_loopback = true;
                i += 1;
            }
            "--allow-funnel" => {
                allow_funnel = true;
                i += 1;
            }
            "--allow-any-origin" => {
                allow_any_origin = true;
                i += 1;
            }
            "--" => {
                herdr_args.extend(args[i + 1..].iter().cloned());
                break;
            }
            other => {
                eprintln!("herdr web: unknown option: {other}");
                return ParseResult::Err(2);
            }
        }
    }

    let bind = match bind_raw.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => {
            eprintln!("herdr web: invalid bind address: {bind_raw}");
            return ParseResult::Err(2);
        }
    };

    ParseResult::Ok(WebConfig {
        bind,
        herdr_bin,
        herdr_args,
        allow_non_loopback,
        allow_funnel,
        allowed_origins,
        allow_any_origin,
    })
}

fn print_web_help() {
    println!("herdr web — serve a phone-friendly xterm.js terminal over a WebSocket");
    println!();
    println!("Usage: herdr web [options] [-- <herdr args>]");
    println!();
    println!("Front this with `tailscale serve` (tailnet-only). It is a full shell.");
    println!();
    println!("Options:");
    println!("  --bind <addr>            loopback addr to listen on (default {DEFAULT_BIND})");
    println!("                           env: HERDR_WEB_BIND");
    println!("  --herdr-bin <path>       herdr binary to spawn per connection");
    println!("                           (default: this binary; env HERDR_WEB_HERDR_BIN)");
    println!("  --allowed-origin <o>     extra allowed WS Origin (repeatable)");
    println!("                           env: HERDR_WEB_ALLOWED_ORIGINS (comma-separated)");
    println!("  --allow-non-loopback     permit a routable --bind (you provide auth)");
    println!("  --allow-funnel           start even if `tailscale funnel` is active");
    println!("  --allow-any-origin       disable the same-origin WS check (unsafe)");
    println!("  -- <herdr args>          args passed to the spawned herdr (e.g. --session)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_absent_is_allowed() {
        assert!(origin_allowed(None, Some("127.0.0.1:7681"), &[], false));
    }

    #[test]
    fn same_origin_is_allowed() {
        assert!(origin_allowed(
            Some("https://sage.tailnet.ts.net"),
            Some("sage.tailnet.ts.net"),
            &[],
            false
        ));
    }

    #[test]
    fn cross_origin_is_rejected() {
        assert!(!origin_allowed(
            Some("https://evil.example.com"),
            Some("sage.tailnet.ts.net"),
            &[],
            false
        ));
    }

    #[test]
    fn allow_listed_origin_is_allowed() {
        let allowed = vec!["https://phone.example".to_string()];
        assert!(origin_allowed(
            Some("https://phone.example"),
            Some("127.0.0.1:7681"),
            &allowed,
            false
        ));
    }

    #[test]
    fn allow_any_bypasses_check() {
        assert!(origin_allowed(
            Some("https://evil.example.com"),
            Some("sage.tailnet.ts.net"),
            &[],
            true
        ));
    }

    #[test]
    fn funnel_active_detects_true_value() {
        let v = serde_json::json!({ "AllowFunnel": { "sage.tailnet.ts.net:443": true } });
        assert!(funnel_active(&v));
    }

    #[test]
    fn funnel_inactive_when_all_false_or_absent() {
        let all_false = serde_json::json!({ "AllowFunnel": { "host:443": false } });
        assert!(!funnel_active(&all_false));
        let absent = serde_json::json!({ "Web": {} });
        assert!(!funnel_active(&absent));
        let null = serde_json::Value::Null;
        assert!(!funnel_active(&null));
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("vendor/xterm.min.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type_for("vendor/xterm.min.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type_for("x.bin"), "application/octet-stream");
    }

    #[test]
    fn embedded_assets_present() {
        assert!(WebAssets::get("index.html").is_some());
        assert!(WebAssets::get("vendor/xterm.min.js").is_some());
        assert!(WebAssets::get("vendor/xterm.min.css").is_some());
        assert!(WebAssets::get("vendor/addon-fit.min.js").is_some());
    }
}
