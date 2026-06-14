//! Spawn mediamtx and keep it alive (restart on crash) until shutdown.

use anyhow::{bail, Result};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;

/// Run mediamtx with `config_path`, restarting it if it crashes, until
/// `shutdown` flips to `true`. Returns `Ok(())` on a clean shutdown.
pub async fn run(
    mediamtx_bin: &Path,
    config_path: &Path,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    if !mediamtx_bin.exists() {
        bail!(
            "mediamtx binary not found at {} — run scripts/fetch-mediamtx.sh",
            mediamtx_bin.display()
        );
    }
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut child = Command::new(mediamtx_bin).arg(config_path).spawn()?;
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                if *shutdown.borrow() {
                    return Ok(());
                }
                eprintln!("[relay] mediamtx exited ({status}); restarting in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Ok(());
                }
            }
        }
    }
}
