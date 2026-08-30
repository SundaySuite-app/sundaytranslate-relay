# SundayTranslate Relay

An on-LAN audio relay for [SundayTranslate](https://translate.sundaysuite.app).
Runs on a laptop on the **same wifi** as the congregation and bundles
[mediamtx](https://github.com/bluenviron/mediamtx) (a WHIP/WHEP SFU) so
interpretation audio fans out **locally** — free, low-latency, and the audio
never leaves the building. Cloudflare's SFU stays the automatic fallback for
listeners on 4G or when no relay is running (handled in the web app, which
dual-publishes).

> **Status:** engine (`src/`) + **Tauri desktop shell** (`src-tauri/` + `ui/`) +
> **cloud enroll broker** (`sundaytranslate` PR #3) — all compile (`cargo check
> --workspace` green, 3 engine tests). **Pending:** fetch the mediamtx binary,
> provision the broker secrets, and **rig-test** against a live mediamtx + phones.
> The WHIP/WHEP audio path (web app, `sundaytranslate` PR #3) is implemented but
> **not yet verified against a live mediamtx**.

## Why a local cert (the linchpin)
The web app loads over `https://translate.sundaysuite.app`, so the browser's
`fetch` to this relay is blocked as *mixed content* unless the relay serves
**valid HTTPS** — and you can't get a public CA cert for a raw `192.168.x.x`.
So the cloud (which owns the `sundaysuite.app` zone) brokers it: the relay sends
its LAN IP + a pairing code to `POST /api/relay/enroll`, the cloud upserts
`<slug>.local.sundaysuite.app → <lan ip>` and returns a TLS cert for that host.
Audio still flows entirely on the LAN; only this one-time control call needs
internet.

## Architecture
```
browser (wifi) ──WHIP/WHEP──► [ this relay: mediamtx, HTTPS ] ── audio stays on LAN
browser (4G)   ──tracks API─► Cloudflare SFU                 (fallback, web app dual-publishes)
```
Engine modules (`src/`): `lan` (LAN IP) · `enroll` (cloud cert broker) ·
`mediamtx` (config render + file layout) · `supervise` (spawn/keep-alive) ·
`register` (tell the session this relay hosts it).

## Build & run (headless, dev)
```bash
cargo check                      # compile the engine
./scripts/fetch-mediamtx.sh      # download the SFU into ./binaries/
RELAY_PAIRING_CODE=...   \
RELAY_SESSION_ID=...      \
RELAY_SESSION_SECRET=...  \
cargo run                        # enroll → start mediamtx → register → Ctrl-C
```
All config is env (see `src/main.rs`). The session id/secret come from the
operator's staff URL (`/o/<id>?...#<secret>`).

## Desktop app (Tauri)
```bash
./scripts/fetch-mediamtx.sh   # SFU binary → ./binaries/
npm install                   # @tauri-apps/cli
npm run dev                   # tauri dev — paste pairing code + operator link, Start
npm run build                 # bundled app (needs signing for distribution)
```
The shell (`src-tauri/` Rust commands `start_relay`/`stop_relay`/`relay_status`
over `relay_core`; `ui/index.html` frontend) is a thin wrapper: paste the
pairing code + operator link, hit **Start**, and it enrolls → starts mediamtx →
registers the relay on the session.

## Platforms, and what that means for dependency alerts
This repo has **no release workflow**. `ci.yml` is the only workflow: one
`ubuntu-latest` job running `cargo fmt`, `cargo clippy --workspace` and
`cargo test --workspace`. Nothing is published, signed or distributed from
here yet — the desktop app is built by hand (`npm run build`), and in practice
that means the operator's macOS or Windows laptop.

Two consequences for triaging security alerts against `Cargo.lock`:

- **`Cargo.lock` is platform-independent; the compiled graph is not.** The
  lockfile stores the union of every target's resolution, so scanners flag
  crates that are never built here. The Tauri GTK/WebKitGTK stack is the usual
  source: `glib`, `gtk`, `atk`, `gdk`, `gio`, `pango`, `soup3`, `webkit2gtk`.
  Check before acting, from the repo root — and note the `--workspace`, because
  the root `Cargo.toml` is both `[workspace]` and `[package]`, so a bare
  `cargo tree -i` only sees the root package:

  ```bash
  cargo tree -i glib --workspace --target aarch64-apple-darwin    --edges normal
  cargo tree -i glib --workspace --target x86_64-pc-windows-msvc  --edges normal
  cargo tree -i glib --workspace --target x86_64-unknown-linux-gnu --edges normal
  ```

  `warning: nothing to print.` means the crate is absent on that target.

- **The headless relay is not where the GUI dependencies live.** The part that
  behaves like a server — the `sundaytranslate-relay` binary and `relay_core`
  — resolves *no* GTK stack on any target, Linux included. The only path to it
  is `sundaytranslate-relay-app` (`src-tauri/`), the desktop shell, on Linux
  targets only. So a Linux-only advisory in that stack does not describe the
  relay's network-facing surface.

Worked example — GHSA-wrw7-89jp-8q8g (`glib` 0.18.5, unsoundness in
`VariantStrIter`'s iterator impls), dismissed 2026-08-30: absent on
`aarch64-apple-darwin` and `x86_64-pc-windows-msvc`, present only on
`x86_64-unknown-linux-gnu` via `gtk 0.18.2 ← tauri 2.11.5`, and unfixable
anyway because `gtk 0.18.2` requires `glib = "^0.18"` (`cargo update -p glib`
locks 0 packages). It is compiled exactly once — in the `ubuntu-latest` CI job
— and that output is never shipped.

**The caveat, stated plainly:** `src-tauri/tauri.conf.json` sets
`"targets": "all"`, so `tauri build` on a Linux box *would* emit deb/AppImage
and link the GTK stack for real. Nothing automates that today, which is the
only reason the reasoning above holds. **If a Linux build or a release
workflow is ever added, re-triage the whole GTK stack as shipped code.**

## Rig-test (the real verification — needs 2 phones on one wifi)
1. Fetch mediamtx; start a SundayTranslate session; run the relay with that
   session's id/secret.
2. Interpreter publishes (the web app dual-publishes → relay via WHIP).
3. A listener on the **same wifi** should pull via WHEP (the listener UI shows
   "🟢 Local") — confirm **zero Cloudflare egress** in the CF dashboard.
4. Same listener on **4G** → falls back to Cloudflare ("☁️ Cloud").
5. Kill the relay mid-session → listener auto-falls-back to Cloudflare.

## Pending (next)
- **Cloud `POST /api/relay/enroll`** broker (DNS A-record + cert). Simplest first
  cut: return a pre-provisioned `*.local.sundaysuite.app` wildcard cert stored as
  a Worker secret (avoids per-device ACME). See the plan's "open questions".
- **mediamtx**: config schema **verified loading on v1.9.3** (`scripts/fetch-mediamtx.sh`).
  WHIP publish auth uses HTTP **Basic** (user `publish`), not Bearer — the web
  client was corrected (`sundaytranslate` PR #3). Cross-origin **CORS verified**:
  mediamtx v1.9.3 returns `access-control-allow-origin: *` and
  `access-control-allow-headers: Authorization, Content-Type, If-Match` on the
  WHIP/WHEP preflight — so the page on `translate.sundaysuite.app` can call the
  relay on `*.local…` with Basic auth, no extra config. Still to confirm at rig:
  that publish auth is actually *enforced* with a real SDP offer (curl with an
  invalid SDP returns 400 regardless, masking the auth result).
- **Tauri UI shell**: Start/Stop, status, QR; reuse SundayRec's sidecar bundling
  + updater + Apple signing.
