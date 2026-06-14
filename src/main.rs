//! SundayTranslate Relay — headless entrypoint.
//!
//! Config via env (the Tauri shell will set these from its UI instead):
//!   RELAY_CLOUD_BASE      default https://translate.sundaysuite.app
//!   RELAY_PAIRING_CODE    required — enrolls this device with the broker
//!   RELAY_SESSION_ID      required — the live session to host
//!   RELAY_SESSION_SECRET  required — the session write secret (#fragment)
//!   RELAY_HTTPS_PORT      default 8889
//!   RELAY_DATA_DIR        default ./.relay-data   (cert/key/config land here)
//!   RELAY_MEDIAMTX_BIN    default ./binaries/mediamtx
//!   RELAY_SLUG            default derived from hostname

use anyhow::{Context, Result};
use relay_core::{enroll, lan, mediamtx, register, supervise};
use std::path::PathBuf;
use tokio::sync::watch;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}
fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cloud = env_or("RELAY_CLOUD_BASE", "https://translate.sundaysuite.app");
    let pairing = env("RELAY_PAIRING_CODE").context("RELAY_PAIRING_CODE is required")?;
    let session_id = env("RELAY_SESSION_ID").context("RELAY_SESSION_ID is required")?;
    let session_secret = env("RELAY_SESSION_SECRET").context("RELAY_SESSION_SECRET is required")?;
    let https_port: u16 = env_or("RELAY_HTTPS_PORT", "8889")
        .parse()
        .context("RELAY_HTTPS_PORT must be a port number")?;
    let data_dir = PathBuf::from(env_or("RELAY_DATA_DIR", "./.relay-data"));
    let mediamtx_bin = PathBuf::from(env_or("RELAY_MEDIAMTX_BIN", "./binaries/mediamtx"));
    let slug = env("RELAY_SLUG").unwrap_or_else(stable_slug);

    // 1. LAN IP.
    let ip = lan::detect_lan_ipv4()?;
    eprintln!("[relay] LAN IP: {ip}");

    // 2. Enroll → host + cert for <slug>.local.sundaysuite.app → this IP.
    eprintln!("[relay] enrolling with {cloud} …");
    let e = enroll::enroll(&cloud, &pairing, &ip.to_string(), &slug).await?;
    eprintln!("[relay] host {} (cert expires {})", e.host, e.expires_at);

    // 3. Render config + lay down cert/key/config.
    tokio::fs::create_dir_all(&data_dir).await?;
    let cert_path = data_dir.join("cert.pem").to_string_lossy().into_owned();
    let key_path = data_dir.join("key.pem").to_string_lossy().into_owned();
    let cfg = mediamtx::MediamtxConfig {
        https_port,
        cert_path,
        key_path,
        publish_secret: Some(session_secret.clone()),
    };
    let config_path = mediamtx::write_files(&data_dir, &e.cert_pem, &e.key_pem, &cfg).await?;

    // 4. Register with the session so on-wifi listeners discover + prefer us.
    let relay_url = format!("https://{}:{}", e.host, https_port);
    register::set_session_relay(
        &cloud,
        &session_id,
        &session_secret,
        Some(relay_url.as_str()),
        Some(e.expires_at.as_str()),
    )
    .await?;
    eprintln!("[relay] registered {relay_url} for session {session_id}");

    // 5. Run mediamtx until Ctrl-C.
    let (tx, rx) = watch::channel(false);
    let supervisor =
        tokio::spawn(async move { supervise::run(&mediamtx_bin, &config_path, rx).await });
    eprintln!("[relay] running — Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;

    eprintln!("[relay] shutting down …");
    let _ = tx.send(true);
    // Best-effort: clear the registration so listeners fall back to the cloud.
    let _ =
        register::set_session_relay(&cloud, &session_id, &session_secret, None, None).await;
    let _ = supervisor.await;
    Ok(())
}

/// A short, stable per-machine slug so the same DNS host/cert is reused across
/// restarts (FNV-1a of the hostname). The Tauri shell will persist a real uuid.
fn stable_slug() -> String {
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
