//! Local control daemon: WebSocket on 127.0.0.1, JSON text frames, protocol v1.
//!
//! Security: loopback bind is not access control. Upgrades carrying an
//! `Origin` header (i.e. from a browser page) are rejected, and the first
//! frame must be a `hello` with the token from the discovery file.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use av_core::{lin_to_cdb, ControlState};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

pub const PROTO: u64 = 1;
const PORT_FIRST: u16 = 8989;
const PORT_LAST: u16 = 8999;
/// Gain change per dial tick, in centi-dB.
const STEP_CDB: i32 = 50;
const STEP_FINE_CDB: i32 = 10;
/// Limits that bound what an unauthenticated local client can make the daemon do.
const MAX_CONNECTIONS: usize = 16;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// State shared by the UI commands, hotkeys, daemon connections and poller.
pub struct Shared {
    pub ctl: Arc<ControlState>,
    /// Bumped on every state change; connections push a snapshot when it moves.
    pub seq: watch::Sender<u64>,
    /// Display meter in centi-dBFS with decay, written by the poller.
    pub meter_cdb: AtomicI32,
    pub clip: AtomicBool,
    /// True while both audio streams are open and healthy.
    pub route_ok: AtomicBool,
    pub port: AtomicU32,
    /// True in direct mode (the app controls the real microphone's level and mute).
    pub direct: AtomicBool,
}

impl Shared {
    pub fn new(ctl: Arc<ControlState>) -> Arc<Self> {
        let (seq, _) = watch::channel(0u64);
        Arc::new(Self {
            ctl,
            seq,
            meter_cdb: AtomicI32::new(-12000),
            clip: AtomicBool::new(false),
            route_ok: AtomicBool::new(false),
            port: AtomicU32::new(0),
            direct: AtomicBool::new(true),
        })
    }

    pub fn bump(&self) {
        self.seq.send_modify(|s| *s += 1);
    }

    pub fn snapshot(&self) -> Value {
        let mute = self.ctl.mute.load(Relaxed);
        json!({
            "t": "state",
            "seq": *self.seq.borrow(),
            "mute": mute,
            // Zoom bridge is not in this build, so the effective state is the app's own.
            "effective": if mute { "muted" } else { "live" },
            "gain_cdb": self.ctl.gain_cdb.load(Relaxed),
            "gain_min_cdb": self.ctl.gain_min.load(Relaxed),
            "gain_max_cdb": self.ctl.gain_max.load(Relaxed),
            "mode": if self.direct.load(Relaxed) { "direct" } else { "cable" },
            "zoom": { "meeting": false, "muted": false, "confidence": "none" },
            "route": if self.route_ok.load(Relaxed) { "ok" } else { "stopped" },
            "xruns": self.ctl.xruns.load(Relaxed),
        })
    }

    fn meter_frame(&self) -> Value {
        json!({
            "t": "meter",
            "peak_dbfs": self.meter_cdb.load(Relaxed) as f64 / 100.0,
            "clip": self.clip.load(Relaxed),
        })
    }

    /// Apply one command. Used by daemon clients; UI and hotkeys call `ctl` directly.
    pub fn apply(&self, name: &str, v: &Value) -> Result<(), String> {
        match name {
            "mute.toggle" => {
                self.ctl.toggle_mute();
            }
            "mute.set" => {
                let on = v["on"].as_bool().ok_or("mute.set needs boolean \"on\"")?;
                self.ctl.mute.store(on, Relaxed);
            }
            "ptt" => {
                let down = v["down"].as_bool().ok_or("ptt needs boolean \"down\"")?;
                self.ctl.mute.store(!down, Relaxed);
            }
            "gain.nudge" => {
                let ticks = v["ticks"]
                    .as_i64()
                    .ok_or("gain.nudge needs integer \"ticks\"")?;
                let step = if v["fine"].as_bool().unwrap_or(false) {
                    STEP_FINE_CDB
                } else {
                    STEP_CDB
                };
                let delta = ticks.clamp(-200, 200) as i32 * step;
                self.ctl.nudge_gain_cdb(delta);
            }
            "gain.set" => {
                let cdb = v["cdb"].as_i64().ok_or("gain.set needs integer \"cdb\"")?;
                self.ctl
                    .set_gain_cdb(cdb.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            }
            other => return Err(format!("unknown command \"{other}\"")),
        }
        self.bump();
        Ok(())
    }
}

pub fn discovery_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("AVcompanion")
}

fn new_token() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("OS random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compare without an early exit, so timing does not reveal how much of a guess matched.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The discovery file holds the access token, so it must be readable by the current user only.
/// On Windows, %APPDATA% is already private to the user. On Unix the mode is set explicitly.
fn write_discovery(port: u16, token: &str) -> std::io::Result<()> {
    let dir = discovery_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon.json");
    let body = json!({ "port": port, "token": token, "proto": PROTO, "pid": std::process::id() })
        .to_string();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        // `mode` only applies when the file is created; tighten an existing one too.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, body)
    }
}

/// Bind, publish the discovery file, and serve until the process exits.
pub async fn run(shared: Arc<Shared>) {
    let mut bound = None;
    for port in PORT_FIRST..=PORT_LAST {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            bound = Some((l, port));
            break;
        }
    }
    let Some((listener, port)) = bound else {
        eprintln!("daemon: no free port in {PORT_FIRST}..={PORT_LAST}");
        return;
    };
    let token = Arc::new(new_token());
    if let Err(e) = write_discovery(port, &token) {
        eprintln!("daemon: could not write discovery file: {e}");
    }
    shared.port.store(port as u32, Relaxed);
    shared.bump();

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Over the cap: drop the connection instead of queueing it.
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let (sh, tk) = (shared.clone(), token.clone());
                tokio::spawn(async move {
                    let _ = serve(stream, sh, tk).await;
                    drop(permit);
                });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

async fn serve(stream: TcpStream, shared: Arc<Shared>, token: Arc<String>) -> Result<(), BoxErr> {
    let reject_browsers = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if req.headers().contains_key("origin") {
            let mut r = ErrorResponse::new(Some("forbidden".to_string()));
            *r.status_mut() = StatusCode::FORBIDDEN;
            Err(r)
        } else {
            Ok(resp)
        }
    };
    let mut cfg = WebSocketConfig::default();
    cfg.max_message_size = Some(MAX_MESSAGE_BYTES);
    cfg.max_frame_size = Some(MAX_MESSAGE_BYTES);
    let ws = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(stream, reject_browsers, Some(cfg)),
    )
    .await??;
    let (mut tx, mut rx) = ws.split();

    // First frame must be a valid hello, within two seconds.
    let hello = match tokio::time::timeout(Duration::from_secs(2), rx.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).ok(),
        _ => None,
    };
    let authed = hello.as_ref().is_some_and(|h| {
        h["t"] == "hello"
            && h["proto"].as_u64() == Some(PROTO)
            && h["token"].as_str().is_some_and(|t| token_eq(t, &token))
    });
    if !authed {
        let _ = tx.close().await;
        return Ok(());
    }
    let want_meter = hello
        .as_ref()
        .is_some_and(|h| h["meter"].as_bool().unwrap_or(false));

    tx.send(Message::Text(shared.snapshot().to_string()))
        .await?;
    let mut seq = shared.seq.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(66));

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let reply = match serde_json::from_str::<Value>(&t) {
                        Ok(v) if v["t"] == "cmd" => {
                            let id = v["id"].clone();
                            let name = v["name"].as_str().unwrap_or("").to_string();
                            if name.starts_with("zoom.") {
                                // Zoom commands press keys and wait for windows: keep them off the async workers.
                                let done = tokio::task::spawn_blocking(move || crate::zoom::command(&name, &v)).await;
                                match done {
                                    Ok(Ok(msg)) => json!({ "t": "ack", "id": id, "ok": true, "msg": msg }),
                                    Ok(Err(e)) => json!({ "t": "ack", "id": id, "ok": false, "err": e }),
                                    Err(_) => json!({ "t": "ack", "id": id, "ok": false, "err": "Zoom command failed unexpectedly" }),
                                }
                            } else {
                                match shared.apply(&name, &v) {
                                    Ok(()) => json!({ "t": "ack", "id": id, "ok": true }),
                                    Err(e) => json!({ "t": "ack", "id": id, "ok": false, "err": e }),
                                }
                            }
                        }
                        _ => json!({ "t": "ack", "id": Value::Null, "ok": false, "err": "expected a cmd frame" }),
                    };
                    tx.send(Message::Text(reply.to_string())).await?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            changed = seq.changed() => {
                if changed.is_err() { break; }
                tx.send(Message::Text(shared.snapshot().to_string())).await?;
            }
            _ = tick.tick(), if want_meter => {
                tx.send(Message::Text(shared.meter_frame().to_string())).await?;
            }
        }
    }
    Ok(())
}

/// Reads the atomics the audio callbacks write and turns them into display
/// values. The audio threads never send anything themselves.
pub async fn poller(
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
    running: impl Fn() -> bool + Send + 'static,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(30));
    let mut clip_hold = 0u32;
    let mut last_ok = false;
    let mut last_xruns = 0u32;
    loop {
        tick.tick().await;
        let peak = lin_to_cdb(shared.ctl.take_peak());
        let decayed = shared.meter_cdb.load(Relaxed) - 60; // 20 dB per second
        shared
            .meter_cdb
            .store(peak.max(decayed).max(-12000), Relaxed);

        if shared.ctl.clipped.swap(false, Relaxed) {
            clip_hold = 17; // about half a second
        }
        shared.clip.store(clip_hold > 0, Relaxed);
        clip_hold = clip_hold.saturating_sub(1);

        let ok = running() && !lost.load(Relaxed);
        let xruns = shared.ctl.xruns.load(Relaxed);
        shared.route_ok.store(ok, Relaxed);
        if ok != last_ok || xruns != last_xruns {
            last_ok = ok;
            last_xruns = xruns;
            shared.bump();
        }
    }
}
