//! SundayTranslate Relay — core engine.
//!
//! Runs on a church laptop on the same wifi as the congregation. Bundles
//! **mediamtx** (a WHIP/WHEP SFU) so interpretation audio fans out locally
//! instead of round-tripping to Cloudflare: free, low-latency, stays in the
//! building. The cloud SFU remains the fallback for 4G listeners (handled in the
//! web app — the publisher dual-publishes).
//!
//! Lifecycle (see `bin/main.rs`):
//!   1. [`lan`] detect this machine's LAN IPv4
//!   2. [`enroll`] ask the cloud broker for `<slug>.local.sundaysuite.app` (an
//!      A-record → our LAN IP) + a TLS cert for it (the browser needs valid
//!      HTTPS to talk to us without mixed-content blocking)
//!   3. [`mediamtx`] render a config (WHIP ingest + WHEP egress, Opus, HTTPS)
//!   4. [`supervise`] spawn + keep mediamtx alive
//!   5. [`register`] tell the session this relay hosts it, so on-wifi listeners
//!      discover + prefer it
//!
//! This crate is pure engine (no UI) so the Tauri shell can drive it directly.

pub mod enroll;
pub mod lan;
pub mod mediamtx;
pub mod register;
pub mod supervise;
