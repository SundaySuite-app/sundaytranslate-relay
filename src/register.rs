//! Register this relay with a session, so the web app's listeners discover it.
//!
//! Calls `POST {cloud_base}/api/sessions/{id}/relay` (added in sundaytranslate
//! PR #3) with the session secret as a Bearer token. Once set, on-wifi listeners
//! resolve `session.localRelayUrl` and prefer the WHEP path; 4G listeners fall
//! back to Cloudflare. Passing `relay_url: None` clears it (relay shutting down).

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::Duration;

/// Ceiling for the whole register round-trip. The clear-on-shutdown call runs on
/// the way out of `stop_relay`, so an unbounded wait here would hang the UI's
/// Stop button on a flaky network.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct RelayBody<'a> {
    relay_url: Option<&'a str>,
    expires_at: Option<&'a str>,
}

/// Register (Some) or clear (None) the relay URL for a session.
pub async fn set_session_relay(
    cloud_base: &str,
    session_id: &str,
    session_secret: &str,
    relay_url: Option<&str>,
    expires_at: Option<&str>,
) -> Result<()> {
    set_session_relay_with_timeout(
        cloud_base,
        session_id,
        session_secret,
        relay_url,
        expires_at,
        REGISTER_TIMEOUT,
    )
    .await
}

/// [`set_session_relay`] with an explicit deadline (tests use a short one).
pub async fn set_session_relay_with_timeout(
    cloud_base: &str,
    session_id: &str,
    session_secret: &str,
    relay_url: Option<&str>,
    expires_at: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let url = format!(
        "{}/api/sessions/{}/relay",
        cloud_base.trim_end_matches('/'),
        session_id
    );
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .context("could not build the HTTP client")?;
    let res = client
        .post(&url)
        .bearer_auth(session_secret)
        .json(&RelayBody {
            relay_url,
            expires_at,
        })
        .send()
        .await
        .with_context(|| format!("could not reach the session API at {url}"))?;
    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("relay register failed: HTTP {code} {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::CannedServer;

    #[tokio::test]
    async fn register_posts_the_secret_as_a_bearer_token() {
        let server = CannedServer::responding(
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        )
        .await;

        set_session_relay_with_timeout(
            &server.base_url,
            "sess-1",
            "sup3rsecret",
            Some("https://r-ab12cd.local.sundaysuite.app:8889"),
            Some("2026-09-01T00:00:00Z"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        let req = server.last_request();
        assert!(req.starts_with("POST /api/sessions/sess-1/relay "), "{req}");
        assert!(req.contains("authorization: Bearer sup3rsecret"), "{req}");
        assert!(
            req.contains(r#""relay_url":"https://r-ab12cd.local.sundaysuite.app:8889""#),
            "{req}"
        );
    }

    #[tokio::test]
    async fn clearing_sends_nulls() {
        let server = CannedServer::responding(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string(),
        )
        .await;

        set_session_relay_with_timeout(
            &server.base_url,
            "sess-1",
            "s",
            None,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        let req = server.last_request();
        assert!(req.contains(r#""relay_url":null"#), "{req}");
    }

    #[tokio::test]
    async fn register_gives_up_on_a_silent_api() {
        let server = CannedServer::silent().await;

        let started = std::time::Instant::now();
        let err = set_session_relay_with_timeout(
            &server.base_url,
            "sess-1",
            "s",
            None,
            None,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert!(
            format!("{err:#}").contains("could not reach the session API"),
            "{err:#}"
        );
    }
}
