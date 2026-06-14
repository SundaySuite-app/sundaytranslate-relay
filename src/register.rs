//! Register this relay with a session, so the web app's listeners discover it.
//!
//! Calls `POST {cloud_base}/api/sessions/{id}/relay` (added in sundaytranslate
//! PR #3) with the session secret as a Bearer token. Once set, on-wifi listeners
//! resolve `session.localRelayUrl` and prefer the WHEP path; 4G listeners fall
//! back to Cloudflare. Passing `relay_url: None` clears it (relay shutting down).

use anyhow::{bail, Result};
use serde::Serialize;

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
    let url = format!(
        "{}/api/sessions/{}/relay",
        cloud_base.trim_end_matches('/'),
        session_id
    );
    let res = reqwest::Client::new()
        .post(&url)
        .bearer_auth(session_secret)
        .json(&RelayBody {
            relay_url,
            expires_at,
        })
        .send()
        .await?;
    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("relay register failed: HTTP {code} {body}");
    }
    Ok(())
}
