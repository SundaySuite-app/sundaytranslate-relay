//! Detect the machine's LAN IPv4 — the address phones on the same wifi reach us
//! at, and the value we register in DNS (`<slug>.local.sundaysuite.app → this`).

use anyhow::{bail, Result};
use std::net::Ipv4Addr;

/// Best-effort primary LAN IPv4. Returns an error on an IPv6-only / link-down
/// machine; the caller surfaces that to the operator ("connect to wifi first").
pub fn detect_lan_ipv4() -> Result<Ipv4Addr> {
    match local_ip_address::local_ip() {
        Ok(std::net::IpAddr::V4(v4)) => {
            if v4.is_loopback() {
                bail!("only a loopback address found — connect to the church wifi");
            }
            Ok(v4)
        }
        Ok(std::net::IpAddr::V6(_)) => {
            bail!("no IPv4 LAN address found (IPv6-only network not supported)")
        }
        Err(e) => bail!("could not determine LAN IP: {e}"),
    }
}
