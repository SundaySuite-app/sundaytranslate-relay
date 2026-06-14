//! Enrollment with the cloud broker.
//!
//! The browser loads the web app over HTTPS, so its `fetch` to this relay would
//! be blocked as mixed content unless we serve **valid HTTPS** on our LAN IP.
//! We can't get a public CA cert for a raw `192.168.x.x`, so the cloud owns the
//! `sundaysuite.app` zone and brokers it: we POST our LAN IP + pairing code, it
//! upserts `<slug>.local.sundaysuite.app → <lan ip>` (CF DNS) and returns a TLS
//! cert+key for that host. Audio still flows entirely on the LAN; only this
//! one-time control call needs internet.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct EnrollReq<'a> {
    pairing_code: &'a str,
    lan_ip: &'a str,
    /// Stable per-device slug so the same host/cert is reused across restarts.
    slug: &'a str,
}

/// What the broker returns: the hostname to serve on + a cert/key for it.
#[derive(Debug, Deserialize)]
pub struct Enrollment {
    /// e.g. `r-ab12cd.local.sundaysuite.app`
    pub host: String,
    /// PEM (may be a fullchain).
    pub cert_pem: String,
    /// PEM private key.
    pub key_pem: String,
    /// ISO-8601 cert expiry; the engine re-enrolls before this.
    pub expires_at: String,
}

/// Call `POST {cloud_base}/api/relay/enroll`.
pub async fn enroll(
    cloud_base: &str,
    pairing_code: &str,
    lan_ip: &str,
    slug: &str,
) -> Result<Enrollment> {
    let url = format!("{}/api/relay/enroll", cloud_base.trim_end_matches('/'));
    let res = reqwest::Client::new()
        .post(&url)
        .json(&EnrollReq {
            pairing_code,
            lan_ip,
            slug,
        })
        .send()
        .await?;
    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("enroll failed: HTTP {code} {body}");
    }
    Ok(res.json::<Enrollment>().await?)
}
