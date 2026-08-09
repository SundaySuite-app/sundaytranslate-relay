//! SundayTranslate Relay — Tauri desktop shell over `relay_core`.
//!
//! Three commands drive the engine: `start_relay` (enroll → write config → spawn
//! mediamtx → register the relay on the session), `stop_relay` (shut mediamtx
//! down + clear the registration so listeners fall back to the cloud), and
//! `relay_status` (for the UI). The running relay (its shutdown channel, the
//! supervisor task and the session info) lives in app state between commands.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use relay_core::{enroll, lan, mediamtx, register, slug, supervise};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;

/// How long we wait for mediamtx to actually be gone before giving up on it.
/// Comfortably above `supervise::SHUTDOWN_GRACE` so the polite SIGTERM path gets
/// its full chance first.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

struct Running {
    shutdown: watch::Sender<bool>,
    /// The supervisor task. Awaiting it is how we know mediamtx has released the
    /// port — without it, Stop-then-Start raced into "address already in use".
    supervisor: tauri::async_runtime::JoinHandle<()>,
    cloud: String,
    session_id: String,
    secret: String,
    relay_url: String,
    host: String,
}

/// Start/stop is a three-state machine rather than an `Option`.
///
/// `start_relay` does a lot of `await`ing (enroll over the network, write files,
/// register) between "is it running?" and "record that it is running". With a
/// bare `Option` a second `start_relay` — a double-click on the button, or the
/// operator retrying while the first attempt is still enrolling — sailed past
/// the check and spawned a *second* mediamtx on the same port. `Starting`
/// reserves the slot for the whole startup, and is rolled back on failure.
#[derive(Default)]
enum Slot {
    #[default]
    Idle,
    Starting,
    Up(Box<Running>),
}

#[derive(Default)]
struct AppState {
    slot: Mutex<Slot>,
}

/// Lock without panicking on poison.
///
/// A poisoned mutex only means some other command panicked while holding it. The
/// state behind it is still the only record we have of a *running mediamtx
/// process*, and panicking here would strand that process with no way to stop it
/// from the UI.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        Self {
            running: false,
            host: None,
            relay_url: None,
            session_id: None,
        }
    }

    fn of(running: &Running) -> Self {
        Self {
            running: true,
            host: Some(running.host.clone()),
            relay_url: Some(running.relay_url.clone()),
            session_id: Some(running.session_id.clone()),
        }
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
    // Reserve the slot *before* the first await, so the check and the claim are
    // one atomic step.
    {
        let mut slot = lock(&state.slot);
        match *slot {
            Slot::Idle => *slot = Slot::Starting,
            Slot::Starting => return Err("already_starting".into()),
            Slot::Up(_) => return Err("already_running".into()),
        }
    }

    match start_engine(&app, args).await {
        Ok(running) => {
            let out = StatusOut::of(&running);
            *lock(&state.slot) = Slot::Up(Box::new(running));
            Ok(out)
        }
        Err(err) => {
            // Roll the reservation back so the operator can fix the pairing code
            // and press Start again.
            *lock(&state.slot) = Slot::Idle;
            Err(err)
        }
    }
}

/// The actual startup, with no access to the state lock: everything here either
/// succeeds and hands back a live `Running`, or cleans up after itself.
async fn start_engine(app: &tauri::AppHandle, args: StartArgs) -> Result<Running, String> {
    let https_port = args.https_port.unwrap_or(8889);
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("relay");
    tokio::fs::create_dir_all(&data_dir)
        .await
        .map_err(|e| e.to_string())?;
    let mediamtx_bin = resolve_mediamtx(app);
    let slug = slug::load_or_create(&data_dir).map_err(|e| e.to_string())?;

    // 1. LAN IP → 2. enroll (host + cert, validated before we write it).
    let ip = lan::detect_lan_ipv4().map_err(|e| e.to_string())?;
    let enrolled = enroll::enroll(&args.cloud_base, &args.pairing_code, &ip.to_string(), &slug)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // 3. Render config + lay down cert/key/config.
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
        .map_err(|e| format!("{e:#}"))?;

    // 4. Spawn mediamtx (kept alive by the supervisor).
    let (tx, rx) = watch::channel(false);
    let supervisor = tauri::async_runtime::spawn(async move {
        if let Err(err) = supervise::run(&mediamtx_bin, &config_path, rx).await {
            eprintln!("[relay] supervisor ended: {err:#}");
        }
    });

    // 5. Register the relay on the session so on-wifi listeners prefer it.
    let relay_url = format!("https://{}:{}", enrolled.host, https_port);
    if let Err(err) = register::set_session_relay(
        &args.cloud_base,
        &args.session_id,
        &args.session_secret,
        Some(&relay_url),
        Some(&enrolled.expires_at),
    )
    .await
    {
        // mediamtx is already holding the port. Failing out without stopping it
        // would leave a relay nobody can see and nobody can stop.
        stop_supervisor(&tx, supervisor).await;
        return Err(format!("{err:#}"));
    }

    Ok(Running {
        shutdown: tx,
        supervisor,
        cloud: args.cloud_base,
        session_id: args.session_id,
        secret: args.session_secret,
        relay_url,
        host: enrolled.host,
    })
}

#[tauri::command]
async fn stop_relay(state: tauri::State<'_, AppState>) -> Result<StatusOut, String> {
    let running = {
        let mut slot = lock(&state.slot);
        match std::mem::take(&mut *slot) {
            Slot::Up(r) => Some(r),
            // A start is mid-flight and owns the slot; it will finish and can
            // then be stopped. Put the reservation back rather than letting the
            // in-flight start commit into a slot we just cleared.
            Slot::Starting => {
                *slot = Slot::Starting;
                return Err("still_starting".into());
            }
            Slot::Idle => None,
        }
    };

    if let Some(r) = running {
        // Clear the registration first so listeners fail over to the cloud while
        // the local relay is still serving, then take mediamtx down.
        let _ = register::set_session_relay(&r.cloud, &r.session_id, &r.secret, None, None).await;
        stop_supervisor(&r.shutdown, r.supervisor).await;
    }
    Ok(StatusOut::idle())
}

/// Signal the supervisor and wait for mediamtx to be gone.
async fn stop_supervisor(
    shutdown: &watch::Sender<bool>,
    supervisor: tauri::async_runtime::JoinHandle<()>,
) {
    let _ = shutdown.send(true);
    if tokio::time::timeout(STOP_TIMEOUT, supervisor)
        .await
        .is_err()
    {
        eprintln!(
            "[relay] mediamtx did not exit within {STOP_TIMEOUT:?} — the port may still be held"
        );
    }
}

#[tauri::command]
fn relay_status(state: tauri::State<'_, AppState>) -> StatusOut {
    match &*lock(&state.slot) {
        Slot::Up(r) => StatusOut::of(r),
        // Startup is not "running" yet — the UI shows its own "Kobler til …".
        Slot::Starting | Slot::Idle => StatusOut::idle(),
    }
}

/// Locate the mediamtx binary: env override, then the bundled sidecar, then the
/// dev `./binaries/mediamtx`.
fn resolve_mediamtx(app: &tauri::AppHandle) -> PathBuf {
    // 1. Explicit override — dev + the test harness.
    if let Ok(p) = std::env::var("RELAY_MEDIAMTX_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // 2. externalBin sidecar (the packaged path). Tauri copies the sidecar next
    //    to the app's OWN executable — `Contents/MacOS/mediamtx` on macOS — with
    //    the target-triple suffix stripped. This is the branch a Finder-launched
    //    .app actually takes; the old build declared no sidecar, so this file
    //    never existed and the app fell through to the dev path below (CWD `/`).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("mediamtx");
            if p.exists() {
                return p;
            }
        }
    }
    // 3. Belt-and-braces: a resources-style layout, should the bundling ever
    //    change to drop it under Contents/Resources instead.
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join("binaries").join("mediamtx");
        if p.exists() {
            return p;
        }
    }
    // 4. Dev clone: `npm run tauri dev` from the repo, with ./binaries populated.
    PathBuf::from("./binaries/mediamtx")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            start_relay,
            stop_relay,
            relay_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running SundayTranslate Relay");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reservation itself, exercised without a Tauri app: the check and the
    /// claim must be one step, so a second caller is turned away *before* it can
    /// start enrolling.
    #[test]
    fn the_slot_can_only_be_reserved_once() {
        let state = AppState::default();

        let claim = |state: &AppState| -> Result<(), &'static str> {
            let mut slot = lock(&state.slot);
            match *slot {
                Slot::Idle => {
                    *slot = Slot::Starting;
                    Ok(())
                }
                Slot::Starting => Err("already_starting"),
                Slot::Up(_) => Err("already_running"),
            }
        };

        assert!(claim(&state).is_ok());
        assert_eq!(claim(&state), Err("already_starting"));
        // Rollback on failure frees it again.
        *lock(&state.slot) = Slot::Idle;
        assert!(claim(&state).is_ok());
    }

    #[test]
    fn a_poisoned_lock_still_yields_the_state() {
        let state = std::sync::Arc::new(AppState::default());
        {
            let poisoner = std::sync::Arc::clone(&state);
            let _ = std::thread::spawn(move || {
                let mut slot = lock(&poisoner.slot);
                *slot = Slot::Starting;
                panic!("poison the mutex");
            })
            .join();
        }
        assert!(state.slot.is_poisoned(), "test did not poison the mutex");
        // The old `.lock().unwrap()` would panic here, taking the command — and
        // any chance of stopping mediamtx — with it.
        assert!(matches!(*lock(&state.slot), Slot::Starting));
    }
}
