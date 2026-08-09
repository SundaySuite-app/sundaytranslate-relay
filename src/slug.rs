//! The stable per-installation slug: the DNS label the broker turns into
//! `<slug>.local.sundaysuite.app` and issues a cert for.
//!
//! This used to be an FNV-1a hash of `$HOSTNAME` / `$COMPUTERNAME`, which is a
//! *shell* variable — GUI apps on macOS are not launched from a shell and do not
//! inherit it. So every installation fell through to the literal `"relay"`,
//! hashed it, and asked the broker for the **same** name: each church's enroll
//! overwrote the previous church's A-record, pointing that DNS name at a LAN IP
//! in a different building.
//!
//! Now the slug is random per installation and persisted next to the cert, so it
//! is stable across restarts (the same host and cert are reused) and unique
//! across machines — which is what the old `stable_slug` comment promised all
//! along.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// File under the relay data dir holding this installation's slug.
const SLUG_FILE: &str = "slug";
/// Prefix, so the DNS label always starts with a letter.
const PREFIX: &str = "r-";
/// Hex digits of randomness. 48 bits: collisions are not a concern at church
/// scale, and the label stays short enough to read out loud.
const HEX_DIGITS: usize = 12;

/// The slug for this installation: `RELAY_SLUG` if set, else the persisted one,
/// else a freshly generated one (which is then persisted).
pub fn load_or_create(dir: &Path) -> Result<String> {
    if let Some(from_env) = std::env::var("RELAY_SLUG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        if !is_valid(&from_env) {
            bail!(
                "RELAY_SLUG must be a DNS label (letters, digits and '-', not starting or ending \
                 with '-'), got {from_env:?}"
            );
        }
        return Ok(from_env);
    }
    load_or_create_in(dir)
}

/// [`load_or_create`] without the environment override.
pub fn load_or_create_in(dir: &Path) -> Result<String> {
    let path = dir.join(SLUG_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if is_valid(existing) {
            return Ok(existing.to_string());
        }
        // Empty or corrupt: fall through and mint a new one rather than asking
        // the broker for a name we cannot serve.
    }
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let slug = generate();
    std::fs::write(&path, format!("{slug}\n"))
        .with_context(|| format!("could not persist the relay slug to {}", path.display()))?;
    Ok(slug)
}

/// A fresh random slug, e.g. `r-3f9a12c07b4e`.
pub fn generate() -> String {
    let bits = random_u64() & 0x0000_ffff_ffff_ffff;
    format!("{PREFIX}{bits:0width$x}", width = HEX_DIGITS)
}

/// Usable as a single DNS label.
fn is_valid(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 63
        && slug.starts_with(|c: char| c.is_ascii_alphabetic())
        && !slug.ends_with('-')
        && slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 64 bits of randomness for a uniqueness token (not a secret — enrollment is
/// authenticated by the pairing code, the slug just has to not collide).
fn random_u64() -> u64 {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 8];
            if f.read_exact(&mut buf).is_ok() {
                return u64::from_le_bytes(buf);
            }
        }
    }
    // Fallback (and the Windows path): `RandomState` is seeded from the OS RNG,
    // mixed with the clock so two calls in the same process differ.
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.write_u64(RandomState::new().build_hasher().finish());
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the old `stable_slug()` produced when `$HOSTNAME` was unset — i.e.
    /// on every GUI launch, on every machine.
    fn old_shared_slug() -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in b"relay" {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("r-{:06x}", h & 0xff_ffff)
    }

    #[test]
    fn generated_slugs_are_valid_dns_labels() {
        for _ in 0..50 {
            let slug = generate();
            assert!(is_valid(&slug), "{slug} is not a usable DNS label");
            assert_eq!(slug.len(), PREFIX.len() + HEX_DIGITS, "{slug}");
        }
    }

    #[test]
    fn generated_slugs_differ_between_installations() {
        let a: std::collections::HashSet<String> = (0..50).map(|_| generate()).collect();
        assert!(a.len() > 45, "generate() is not random enough: {a:?}");
    }

    /// The bug this replaces: every machine asking for the same name.
    #[test]
    fn generated_slugs_are_never_the_old_shared_one() {
        let old = old_shared_slug();
        for _ in 0..50 {
            assert_ne!(generate(), old);
        }
    }

    #[test]
    fn the_slug_is_stable_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_in(dir.path()).unwrap();
        for _ in 0..5 {
            assert_eq!(load_or_create_in(dir.path()).unwrap(), first);
        }
        // Persisted where the next launch will find it.
        let on_disk = std::fs::read_to_string(dir.path().join(SLUG_FILE)).unwrap();
        assert_eq!(on_disk.trim(), first);
    }

    #[test]
    fn a_missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("relay").join("deep");
        let slug = load_or_create_in(&nested).unwrap();
        assert_eq!(load_or_create_in(&nested).unwrap(), slug);
    }

    #[test]
    fn a_corrupt_slug_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        for junk in [
            "",
            "   \n",
            "not a dns label!",
            "-leading-dash",
            "9starts-with-digit",
        ] {
            std::fs::write(dir.path().join(SLUG_FILE), junk).unwrap();
            let slug = load_or_create_in(dir.path()).unwrap();
            assert!(is_valid(&slug), "junk {junk:?} produced {slug:?}");
            assert_ne!(slug.trim(), junk.trim());
        }
    }

    #[test]
    fn validation_matches_dns_label_rules() {
        assert!(is_valid("r-3f9a12c07b4e"));
        assert!(is_valid("relay1"));
        assert!(!is_valid(""));
        assert!(!is_valid("-nope"));
        assert!(!is_valid("nope-"));
        assert!(!is_valid("has.dot"));
        assert!(!is_valid("has space"));
        assert!(!is_valid("1leading-digit"));
        assert!(!is_valid(&"a".repeat(64)));
    }
}
