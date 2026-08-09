//! Enrollment with the cloud broker.
//!
//! The browser loads the web app over HTTPS, so its `fetch` to this relay would
//! be blocked as mixed content unless we serve **valid HTTPS** on our LAN IP.
//! We can't get a public CA cert for a raw `192.168.x.x`, so the cloud owns the
//! `sundaysuite.app` zone and brokers it: we POST our LAN IP + pairing code, it
//! upserts `<slug>.local.sundaysuite.app → <lan ip>` (CF DNS) and returns a TLS
//! cert+key for that host. Audio still flows entirely on the LAN; only this
//! one-time control call needs internet.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Ceiling for the whole enroll round-trip (connect + request + response body).
/// Without it a broker that accepts the connection and then goes quiet wedges
/// startup *forever*, with the UI stuck on "Kobler til …".
pub const ENROLL_TIMEOUT: Duration = Duration::from_secs(10);

/// The zone the broker hands out names in. Anything else means we are talking to
/// something that is not our broker, and we must not write its cert to disk.
const RELAY_HOST_SUFFIX: &str = ".local.sundaysuite.app";

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

impl Enrollment {
    /// Reject a response we cannot actually serve from.
    ///
    /// `serde` only proves the four fields *exist*; a broker bug (or a captive
    /// portal answering with a 200) can hand back empty strings. Writing an empty
    /// `cert.pem` is the worst outcome: mediamtx starts, fails to load the cert,
    /// exits, and the supervisor restarts it in a loop — a crash-loop whose real
    /// cause is three steps upstream. Fail here, where the message is useful.
    pub fn validate(&self) -> Result<()> {
        let host = self.host.trim();
        if host.is_empty() {
            bail!("broker returned an empty host — check the pairing code");
        }
        if !host.ends_with(RELAY_HOST_SUFFIX) {
            bail!("broker returned host {host:?}, which is not a {RELAY_HOST_SUFFIX} name");
        }
        // `<label>` + suffix — the suffix alone is not a usable host.
        if host.len() == RELAY_HOST_SUFFIX.len() {
            bail!("broker returned host {host:?} with no device label");
        }
        if !self.cert_pem.contains("-----BEGIN") {
            bail!("broker returned a certificate that is not PEM (no -----BEGIN marker)");
        }
        if !self.key_pem.contains("-----BEGIN") {
            bail!("broker returned a private key that is not PEM (no -----BEGIN marker)");
        }
        Ok(())
    }
}

/// Call `POST {cloud_base}/api/relay/enroll`.
pub async fn enroll(
    cloud_base: &str,
    pairing_code: &str,
    lan_ip: &str,
    slug: &str,
) -> Result<Enrollment> {
    enroll_with_timeout(cloud_base, pairing_code, lan_ip, slug, ENROLL_TIMEOUT).await
}

/// [`enroll`] with an explicit deadline (tests use a short one).
pub async fn enroll_with_timeout(
    cloud_base: &str,
    pairing_code: &str,
    lan_ip: &str,
    slug: &str,
    timeout: Duration,
) -> Result<Enrollment> {
    let url = format!("{}/api/relay/enroll", cloud_base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .context("could not build the HTTP client")?;
    let res = client
        .post(&url)
        .json(&EnrollReq {
            pairing_code,
            lan_ip,
            slug,
        })
        .send()
        .await
        .with_context(|| format!("could not reach the broker at {url}"))?;
    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("enroll failed: HTTP {code} {body}");
    }
    // NOTE: `timeout` above covers the body read too, so a broker that sends
    // headers and then stalls cannot hang us here either.
    let enrollment = res
        .json::<Enrollment>()
        .await
        .context("broker response was not a valid enrollment")?;
    enrollment.validate()?;
    Ok(enrollment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::CannedServer;

    fn valid() -> Enrollment {
        Enrollment {
            host: "r-ab12cd.local.sundaysuite.app".into(),
            cert_pem: "-----BEGIN CERTIFICATE-----\nMII…\n-----END CERTIFICATE-----\n".into(),
            key_pem: "-----BEGIN PRIVATE KEY-----\nMII…\n-----END PRIVATE KEY-----\n".into(),
            expires_at: "2026-09-01T00:00:00Z".into(),
        }
    }

    fn json_200(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[test]
    fn validate_accepts_a_real_enrollment() {
        valid().validate().unwrap();
    }

    #[test]
    fn validate_rejects_an_empty_host() {
        let mut e = valid();
        e.host = String::new();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("empty host"), "{err}");
    }

    #[test]
    fn validate_rejects_a_foreign_host() {
        let mut e = valid();
        e.host = "relay.evil.example".into();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("local.sundaysuite.app"), "{err}");
    }

    #[test]
    fn validate_rejects_a_bare_suffix() {
        let mut e = valid();
        e.host = ".local.sundaysuite.app".into();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("no device label"), "{err}");
    }

    #[test]
    fn validate_rejects_an_empty_cert() {
        let mut e = valid();
        e.cert_pem = String::new();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("certificate"), "{err}");
    }

    #[test]
    fn validate_rejects_a_non_pem_key() {
        let mut e = valid();
        e.key_pem = "not a key".into();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("private key"), "{err}");
    }

    #[tokio::test]
    async fn enroll_posts_the_pairing_payload_and_returns_the_enrollment() {
        let body = r#"{"host":"r-ab12cd.local.sundaysuite.app","cert_pem":"-----BEGIN CERTIFICATE-----\nx\n","key_pem":"-----BEGIN PRIVATE KEY-----\ny\n","expires_at":"2026-09-01T00:00:00Z"}"#;
        let server = CannedServer::responding(json_200(body)).await;

        let e = enroll_with_timeout(
            &server.base_url,
            "PAIR-123",
            "192.168.1.42",
            "r-ab12cd",
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(e.host, "r-ab12cd.local.sundaysuite.app");
        let req = server.last_request();
        assert!(req.starts_with("POST /api/relay/enroll "), "{req}");
        assert!(req.contains(r#""pairing_code":"PAIR-123""#), "{req}");
        assert!(req.contains(r#""lan_ip":"192.168.1.42""#), "{req}");
    }

    /// A 200 with an empty cert used to be written straight to disk, and mediamtx
    /// then crash-looped every 2 seconds with no hint why.
    #[tokio::test]
    async fn enroll_rejects_a_200_with_an_empty_cert() {
        let body = r#"{"host":"r-ab12cd.local.sundaysuite.app","cert_pem":"","key_pem":"","expires_at":""}"#;
        let server = CannedServer::responding(json_200(body)).await;

        let err = enroll_with_timeout(
            &server.base_url,
            "p",
            "192.168.1.42",
            "s",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("certificate"), "{err}");
    }

    #[tokio::test]
    async fn enroll_surfaces_broker_errors() {
        let server = CannedServer::responding(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 13\r\nConnection: close\r\n\r\nbad pair code".to_string(),
        )
        .await;

        let err = enroll_with_timeout(
            &server.base_url,
            "nope",
            "192.168.1.42",
            "s",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("bad pair code"), "{err}");
    }

    /// A broker that accepts the TCP connection and then says nothing used to
    /// hang startup forever.
    #[tokio::test]
    async fn enroll_gives_up_on_a_silent_broker() {
        let server = CannedServer::silent().await;

        let started = std::time::Instant::now();
        let err = enroll_with_timeout(
            &server.base_url,
            "p",
            "192.168.1.42",
            "s",
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
            format!("{err:#}").contains("could not reach the broker"),
            "{err:#}"
        );
    }
}
