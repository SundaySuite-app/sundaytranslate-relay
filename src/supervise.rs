//! Spawn mediamtx and keep it alive (restart on crash) until shutdown.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// Delay before the first restart; doubles per consecutive crash.
pub const MIN_RESTART_DELAY: Duration = Duration::from_secs(2);
/// Ceiling for the restart delay. A permanently broken config (bad port, bad
/// cert) used to respawn every 2s forever, spamming the log and the CPU.
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// A process that stayed up this long counts as healthy: the *next* crash starts
/// the backoff over, so an evening-long service that hiccups twice does not
/// inherit a 30s penalty from something that happened at soundcheck.
pub const STABLE_UPTIME: Duration = Duration::from_secs(60);
/// How long mediamtx gets to shut down on SIGTERM before we SIGKILL it.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Restart policy, extracted so it can be tested without spawning anything.
///
/// Given the current consecutive-failure count and how long the process that
/// just died had been up, return the new failure count and how long to wait
/// before respawning.
pub fn next_restart(consecutive_failures: u32, uptime: Duration) -> (u32, Duration) {
    let failures = if uptime >= STABLE_UPTIME {
        // It ran fine and then died — treat this as the first failure again.
        1
    } else {
        consecutive_failures.saturating_add(1)
    };
    // 2s, 4s, 8s, 16s, 30s (capped), …
    let shift = failures.saturating_sub(1).min(16);
    let delay = MIN_RESTART_DELAY.saturating_mul(2u32.saturating_pow(shift));
    (failures, delay.min(MAX_RESTART_DELAY))
}

/// Why we stopped waiting on the current child.
enum Wake {
    Exited(ExitStatus),
    Stop,
    /// `shutdown` changed but is still `false` — keep waiting on the same child.
    Spurious,
}

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
    let mut failures: u32 = 0;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut child = Command::new(mediamtx_bin)
            .arg(config_path)
            // Safety net: if this task is cancelled (or unwinds) the child dies
            // with it instead of being orphaned holding :8889.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start mediamtx at {}", mediamtx_bin.display()))?;
        let started = Instant::now();

        // Wait on *this* child. Note the select only computes a `Wake`; acting on
        // the child happens after, once the `child.wait()` future is dropped.
        let wake = loop {
            let wake = tokio::select! {
                status = child.wait() => Wake::Exited(status.context("waiting for mediamtx failed")?),
                changed = shutdown.changed() => match changed {
                    Ok(()) if *shutdown.borrow() => Wake::Stop,
                    // The sender was dropped, so nobody can ever signal us again
                    // — and `changed()` now returns `Err` *instantly*, forever.
                    // Falling through here spun the outer loop at full speed,
                    // spawning a new mediamtx per turn while the previous child
                    // was dropped un-killed. Treat a dead sender as shutdown.
                    Err(_) => Wake::Stop,
                    // Woken with the flag still false: do NOT fall through to a
                    // respawn — the child from this iteration is still running.
                    Ok(()) => Wake::Spurious,
                },
            };
            match wake {
                Wake::Spurious => continue,
                other => break other,
            }
        };

        match wake {
            Wake::Stop => {
                stop(&mut child).await;
                return Ok(());
            }
            Wake::Exited(status) => {
                if *shutdown.borrow() {
                    return Ok(());
                }
                let uptime = started.elapsed();
                let (new_failures, delay) = next_restart(failures, uptime);
                failures = new_failures;
                eprintln!(
                    "[relay] mediamtx exited ({status}) after {uptime:?}; \
                     restart #{failures} in {delay:?}"
                );
                // Sleep, but stay interruptible: shutdown during the backoff must
                // not wait out a 30s penalty.
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                }
            }
            Wake::Spurious => unreachable!("spurious wakes are handled in the inner loop"),
        }
    }
}

/// Ask mediamtx to exit politely, then insist.
///
/// `Child::start_kill` is an unconditional SIGKILL, which gives mediamtx no
/// chance to close its WebRTC listeners — the next start then trips over
/// "address already in use". SIGTERM first, SIGKILL only if it will not go.
async fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: we still own `child` and have not reaped it, so `pid` is either
        // live or a zombie — it cannot have been recycled by another process.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
            .await
            .is_ok()
        {
            return;
        }
        eprintln!("[relay] mediamtx ignored SIGTERM for {SHUTDOWN_GRACE:?} — killing it");
    }
    // Windows has no SIGTERM; `start_kill` (TerminateProcess) is the only option
    // there, so the graceful path above is unix-only by design.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        // Each crash comes quickly after the last, so failures accumulate.
        let quick = Duration::from_secs(1);
        let (f, d) = next_restart(0, quick);
        assert_eq!((f, d), (1, Duration::from_secs(2)));
        let (f, d) = next_restart(f, quick);
        assert_eq!((f, d), (2, Duration::from_secs(4)));
        let (f, d) = next_restart(f, quick);
        assert_eq!((f, d), (3, Duration::from_secs(8)));
        let (f, d) = next_restart(f, quick);
        assert_eq!((f, d), (4, Duration::from_secs(16)));
        let (f, d) = next_restart(f, quick);
        assert_eq!((f, d), (5, MAX_RESTART_DELAY));
        let (_, d) = next_restart(50, quick);
        assert_eq!(
            d, MAX_RESTART_DELAY,
            "never exceeds the cap, never overflows"
        );
    }

    #[test]
    fn stable_uptime_resets_the_backoff() {
        let (f, d) = next_restart(9, STABLE_UPTIME);
        assert_eq!((f, d), (1, MIN_RESTART_DELAY));
        // One second short still counts as flapping.
        let (f, _) = next_restart(9, STABLE_UPTIME - Duration::from_secs(1));
        assert_eq!(f, 10);
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_not_spawned() {
        let (_tx, rx) = watch::channel(false);
        let err = run(
            Path::new("/nonexistent/mediamtx"),
            Path::new("/nonexistent/mediamtx.yml"),
            rx,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("fetch-mediamtx.sh"), "{err}");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::path::PathBuf;

        /// A stand-in for mediamtx.
        ///
        /// `run()` invokes `<bin> <config>`, so `bin` is a **symlink to /bin/sh**
        /// and the "config" is the script: appends a line to `log` on every start,
        /// then runs for `seconds` (0 = crash immediately). Pointing at an
        /// existing interpreter rather than exec'ing a file we just wrote avoids
        /// an intermittent macOS ETXTBSY race that made these tests flaky.
        fn fake_mediamtx(dir: &Path, log: &Path, seconds: u32) -> (PathBuf, PathBuf) {
            let bin = dir.join("fake-mediamtx");
            if !bin.exists() {
                std::os::unix::fs::symlink("/bin/sh", &bin).unwrap();
            }
            let config = dir.join("mediamtx.yml");
            std::fs::write(
                &config,
                format!(
                    "echo started >> {log}\nexec sleep {seconds}\n",
                    log = log.display(),
                ),
            )
            .unwrap();
            (bin, config)
        }

        fn starts(log: &Path) -> usize {
            std::fs::read_to_string(log)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }

        /// Give the child a beat to actually reach its first line.
        async fn settle() {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }

        #[tokio::test]
        async fn shutdown_stops_the_child_and_returns() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("starts.log");
            let (bin, config) = fake_mediamtx(dir.path(), &log, 60);

            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { run(&bin, &config, rx).await });

            settle().await;
            assert_eq!(starts(&log), 1, "the fake mediamtx never started");
            tx.send(true).unwrap();

            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run() did not return after shutdown")
                .unwrap()
                .unwrap();
            assert_eq!(starts(&log), 1, "shutdown must not respawn");
        }

        /// The regression that mattered most: dropping the sender (which is what
        /// happened when `start_relay` failed after spawning the supervisor) made
        /// `changed()` return `Err` instantly, and the old loop answered that by
        /// spawning another mediamtx — over and over, each one orphaned.
        #[tokio::test]
        async fn a_dropped_sender_stops_instead_of_spawning_forever() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("starts.log");
            let (bin, config) = fake_mediamtx(dir.path(), &log, 60);

            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { run(&bin, &config, rx).await });

            settle().await;
            assert_eq!(starts(&log), 1, "the fake mediamtx never started");
            drop(tx);

            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run() spun instead of returning when the sender dropped")
                .unwrap()
                .unwrap();
            assert_eq!(
                starts(&log),
                1,
                "a dead sender must not trigger a respawn storm"
            );
        }

        /// A `false` on the channel is not a shutdown and must not respawn.
        #[tokio::test]
        async fn a_false_signal_keeps_the_same_child() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("starts.log");
            let (bin, config) = fake_mediamtx(dir.path(), &log, 60);

            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { run(&bin, &config, rx).await });

            settle().await;
            tx.send(false).unwrap();
            settle().await;
            assert_eq!(starts(&log), 1, "a `false` signal respawned mediamtx");

            tx.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run() did not return")
                .unwrap()
                .unwrap();
        }

        /// A crash *is* restarted — after the backoff, not instantly.
        #[tokio::test]
        async fn a_crash_is_restarted_after_the_backoff() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("starts.log");
            let (bin, config) = fake_mediamtx(dir.path(), &log, 0); // exits at once

            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { run(&bin, &config, rx).await });

            // Well inside the 2s first backoff: exactly one start so far.
            settle().await;
            assert_eq!(starts(&log), 1, "restarted without waiting out the backoff");

            // Past it: restarted once.
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert_eq!(starts(&log), 2, "did not restart after the backoff");

            tx.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run() did not return")
                .unwrap()
                .unwrap();
        }

        /// Shutdown during the restart backoff must not wait the backoff out.
        #[tokio::test]
        async fn shutdown_interrupts_the_backoff() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("starts.log");
            let (bin, config) = fake_mediamtx(dir.path(), &log, 0);

            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { run(&bin, &config, rx).await });

            settle().await; // now inside the 2s backoff
            let asked = Instant::now();
            tx.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(2), handle)
                .await
                .expect("run() waited out the backoff before shutting down")
                .unwrap()
                .unwrap();
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "{:?}",
                asked.elapsed()
            );
        }
    }
}
