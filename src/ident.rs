//! Change identity: a stable `Queue-Id:` trailer carried in commit messages.
//!
//! Commit SHAs are worthless identifiers in a rewrite-heavy workflow — every
//! amend, move and requeue mints new ones. The message, however, is carried
//! faithfully through rebase, cherry-pick, replay and amend, so a trailer in
//! it gives each *change* a stable identity across rewrites (the same idea as
//! Gerrit's `Change-Id`). git-queue uses it to tell "the remote has new
//! teammate work" apart from "the remote is a stale copy of our own commits",
//! and to detect squash-merged work whose SHAs and patch-ids are gone.

use std::time::{SystemTime, UNIX_EPOCH};

/// The trailer key, as it appears in commit messages: `Queue-Id: q-...`.
pub const TRAILER: &str = "Queue-Id";

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Mint a new id: `q-` + 26 base32 chars (48-bit millisecond timestamp +
/// 80 random bits, ULID-style). Sortable, unique, and dependency-free.
pub fn new_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut bits: Vec<u8> = Vec::with_capacity(16);
    bits.extend_from_slice(&ms.to_be_bytes()[2..]); // low 48 bits of the time
    bits.extend_from_slice(&random_bytes());
    encode(&bits)
}

/// 80 bits of randomness: the OS pool when available, else a hash of
/// high-resolution time, pid and a counter (uniqueness, not secrecy).
fn random_bytes() -> [u8; 10] {
    use std::io::Read;
    let mut out = [0u8; 10];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    fallback_random()
}

fn fallback_random() -> [u8; 10] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut h);
    let a = h.finish();
    h.write_u64(a);
    let b = h.finish();
    let mut out = [0u8; 10];
    out[..8].copy_from_slice(&a.to_be_bytes());
    out[8..].copy_from_slice(&b.to_be_bytes()[..2]);
    out
}

/// Crockford-base32 encode 16 bytes (128 bits) into `q-` + 26 chars.
fn encode(bytes: &[u8]) -> String {
    let mut acc: u128 = 0;
    for b in bytes {
        acc = (acc << 8) | *b as u128;
    }
    let mut chars = [b'0'; 26];
    for i in (0..26).rev() {
        chars[i] = CROCKFORD[(acc & 31) as usize];
        acc >>= 5;
    }
    format!("q-{}", std::str::from_utf8(&chars).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_wellformed_and_sortable() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert_eq!(id.len(), 28, "{id}");
            assert!(id.starts_with("q-"));
            assert!(id[2..].bytes().all(|c| CROCKFORD.contains(&c)), "{id}");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = new_id();
        assert!(c > a, "ids must sort by creation time: {a} !< {c}");
    }
}
