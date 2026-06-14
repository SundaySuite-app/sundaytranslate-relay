//! SundayTranslate Relay — Tauri desktop shell over `relay_core`.
//!
//! Three commands drive the engine: `start_relay` (enroll → write config → spawn
//! mediamtx → register the relay on the session), `stop_relay` (shut mediamtx
//! down + clear the registration so listeners fall back to the cloud), and
//! `relay_status` (for the UI). The running relay (its shutdown channel + session
//! info) lives in app state between commands.

use std::path::PathBuf;
use std::sync::Mutex;

use relay_core::{enroll, lan, mediamtx, register, supervise};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;

struct Running {
    shutdown: watch::Sender<bool>,
    cloud: String,
    session_id: String,
    secret: String,
    relay_url: String,
    host: String,
}

#[derive(Default)]
struct AppState {
    running: Mutex<Option<Running>>,
}

#[derive(Serialize)]
struct StatusOut {
    running: bool,
    host: Option<String>,
    relay_url: Option<String>,
    session_id: Option<String>,
}

impl StatusOut {
    fn idle() -> Self {
        Self { running: false, host: None, relay_url: None, session_id: None }
    }
}

#[derive(Deserialize)]
struct StartArgs {
    cloud_base: String,
    pairing_code: String,
    session_id: String,
    session_secret: String,
    https_port: Option<u16>,
}

#[tauri::command]
async fn start_relay(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: StartArgs,
) -> Result<StatusOut, String> {
    if state.running.lock().unwrap().is_some() {
        return Err("already_running".into());
    }
    let https_port = args.https_port.unwrap_or(8889);
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("relay");
    let mediamtx_bin = resolve_mediamtx(&app);
    let slug = slug_for_host();

    // 1. LAN IP → 2. enroll (host + wildcard cert).
    let ip = lan::detect_lan_ipv4().map_err(|e| e.to_string())?;
    let enrolled = enroll::enroll(&args.cloud_base, &args.pairing_code, &ip.to_string(), &slug)
        .await
        .map_err(|e| e.to_string())?;

    // 3. Render config + lay down cert/key/config.
    tokio::fs::create_dir_all(&data_dir).await.map_err(|e| e.to_string())?;
    let cert_path = data_dir.join("cert.pem").to_string_lossy().into_owned();
    let key_path = data_dir.join("key.pem").to_string_lossy().into_owned();
    let cfg = mediamtx::MediamtxConfig {
        https_port,
        cert_path,
        key_path,
        publish_secret: Some(args.session_secret.clone()),
    };
    let config_path = mediamtx::write_files(&data_dir, &enrolled.cert_pem, &enrolled.key_pem, &cfg)
        .await
        .map_err(|e| e.to_string())?;

    // 4. Spawn mediamtx (kept alive by the supervisor).
    let (tx, rx) = watch::channel(false);
    tauri::async_runtime::spawn(async move {
        if let Err(err) = supervise::run(&mediamtx_bin, &config_path, rx).await {
            eprintln!("[relay] supervisor ended: {err}");
        }
    });

    // 5. Register the relay on the session so on-wifi listeners prefer it.
    let relay_url = format!("https://{}:{}", enrolled.host, https_port);
    register::set_session_relay(
        &args.cloud_base,
        &args.session_id,
        &args.session_secret,
        Some(&relay_url),
        Some(&enrolled.expires_at),
    )
    .await
    .map_err(|e| e.to_string())?;

    let out = StatusOut {
        running: true,
        host: Some(enrolled.host.clone()),
        relay_url: Some(relay_url.clone()),
        session_id: Some(args.session_id.clone()),
    };
    *state.running.lock().unwrap() = Some(Running {
        shutdown: tx,
        cloud: args.cloud_base,
        session_id: args.session_id,
        secret: args.session_secret,
        relay_url,
        host: enrolled.host,
    });
    Ok(out)
}

#[tauri::command]
async fn stop_relay(state: tauri::State<'_, AppState>) -> Result<StatusOut, String> {
    let running = state.running.lock().unwrap().take();
    if let Some(r) = running {
        let _ = r.shutdown.send(true);
        // Best-effort: clear the registration so listeners fall back to cloud.
        let _ = register::set_session_relay(&r.cloud, &r.session_id, &r.secret, None, None).await;
    }
    Ok(StatusOut::idle())
}

#[tauri::command]
fn relay_status(state: tauri::State<'_, AppState>) -> StatusOut {
    match &*state.running.lock().unwrap() {
        Some(r) => StatusOut {
            running: true,
            host: Some(r.host.clone()),
            relay_url: Some(r.relay_url.clone()),
            session_id: Some(r.session_id.clone()),
        },
        None => StatusOut::idle(),
    }
}

/// Locate the mediamtx binary: env override, then the bundled sidecar, then the
/// dev `./binaries/mediamtx`.
fn resolve_mediamtx(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("RELAY_MEDIAMTX_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join("binaries").join("mediamtx");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("./binaries/mediamtx")
}

/// Stable per-machine slug (FNV-1a of the hostname) so the same DNS host/cert is
/// reused across restarts.
fn slug_for_host() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "relay".to_string());
    let mut h: u64 = 0xcbf29ce484222325;
    for b in host.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("r-{:06x}", h & 0xff_ffff)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![start_relay, stop_relay, relay_status])
        .run(tauri::generate_context!())
        .expect("error while running SundayTranslate Relay");
}
