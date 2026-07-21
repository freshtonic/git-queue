//! Change identity: a stable `Queued-Commit-Id:` trailer carried in commit messages.
//!
//! Commit SHAs are worthless identifiers in a rewrite-heavy workflow — every
//! amend, move and requeue mints new ones. The message, however, is carried
//! faithfully through rebase, cherry-pick, replay and amend, so a trailer in
//! it gives each *change* a stable identity across rewrites (the same idea as
//! Gerrit's `Change-Id`). git-queue uses it to tell "the remote has new
//! teammate work" apart from "the remote is a stale copy of our own commits",
//! and to detect squash-merged work whose SHAs and patch-ids are gone.

use std::time::{SystemTime, UNIX_EPOCH};

/// The trailer key, as it appears in commit messages: `Queued-Commit-Id: q-...`.
pub const TRAILER: &str = "Queued-Commit-Id";

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Mint a new id: `q-` + 26 base32 chars of pure randomness (128 bits).
/// Uniform from the first character, so prefix abbreviations (as shown by
/// `git queue log`) are high-entropy, like git's own hash abbreviations.
/// Identity is the only job — git already records when a commit was made.
pub fn new_id() -> String {
    encode(&random_bytes())
}

/// 128 bits of randomness: the OS pool when available, else a hash of
/// high-resolution time, pid and a counter (uniqueness, not secrecy).
fn random_bytes() -> [u8; 16] {
    use std::io::Read;
    let mut out = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    fallback_random()
}

fn fallback_random() -> [u8; 16] {
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
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_be_bytes());
    out[8..].copy_from_slice(&b.to_be_bytes());
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
    fn ids_are_unique_wellformed_and_prefix_diverse() {
        let ids: Vec<String> = (0..64).map(|_| new_id()).collect();
        for id in &ids {
            assert_eq!(id.len(), 28, "{id}");
            assert!(id.starts_with("q-"));
            assert!(id[2..].bytes().all(|c| CROCKFORD.contains(&c)), "{id}");
        }
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids must be unique");
        // The abbreviated display prefix (q- + 8 chars) must be high-entropy:
        // no collisions among 64 ids is overwhelmingly likely at 40 bits.
        let prefixes: std::collections::HashSet<&str> = ids.iter().map(|i| &i[..10]).collect();
        assert_eq!(prefixes.len(), ids.len(), "prefixes collided: {ids:?}");
    }
}
