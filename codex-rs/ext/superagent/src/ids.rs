//! Stable content-derived identifiers; equal inputs always produce the same id (prototype `src/core/ids.ts`).

use sha2::Digest;
use sha2::Sha256;

/// The first 32 hex characters of the SHA-256 of the values as a JSON array.
pub fn key<I, S>(values: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let values: Vec<String> = values.into_iter().map(|v| v.as_ref().to_owned()).collect();
    let json = serde_json::to_string(&values).unwrap_or_default();
    let digest = Sha256::digest(json.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

