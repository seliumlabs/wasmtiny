//! Integrity verification: scheme dispatch for the mandatory integrity section.

use crate::runtime::{Result, WasmError};

use super::format::{INTEGRITY_SHA512, SHA512_LEN};

/// Verifies an integrity-section payload against the bytes it covers.
///
/// The payload is `scheme | key_id_len | key_id | digest`; the digest covers
/// every byte preceding the digest itself — `preceding` (the whole artifact
/// through the section header) plus the payload bytes before the digest (the
/// scheme and key-id fields). Verification is fail-closed: a missing,
/// unknown-scheme, wrongly-keyed, or non-matching digest is a hard refusal.
///
/// The `key_id_len` field is the reserved extension point for future
/// signature schemes (PKI): a new scheme reuses the same section shape with
/// its own key-id policy, requiring no format version bump. The v1 SHA512
/// scheme is unkeyed, so it requires an empty key id.
pub fn verify_integrity(preceding: &[u8], payload: &[u8]) -> Result<()> {
    let scheme = *payload
        .first()
        .ok_or_else(|| WasmError::Load("integrity section has an empty payload".to_string()))?;

    match scheme {
        INTEGRITY_SHA512 => {
            use sha2::{Digest, Sha512};
            let key_id_len = *payload.get(1).ok_or_else(|| {
                WasmError::Load("integrity payload lacks a key-id length".to_string())
            })? as usize;
            if key_id_len != 0 {
                return Err(WasmError::Load(format!(
                    "integrity scheme {INTEGRITY_SHA512:#04x} (SHA512) is unkeyed but declares \
                     a {key_id_len}-byte key id"
                )));
            }
            let digest_start = 2 + key_id_len;
            let digest = payload
                .get(digest_start..digest_start + SHA512_LEN)
                .ok_or_else(|| {
                    WasmError::Load(format!(
                        "integrity payload too short for SHA512 ({} bytes)",
                        payload.len().saturating_sub(digest_start)
                    ))
                })?;
            // The digest covers everything before it: the preceding bytes
            // plus the scheme/key-id fields of this payload.
            let mut hasher = Sha512::new();
            hasher.update(preceding);
            hasher.update(&payload[..digest_start]);
            let expected = hasher.finalize();
            if digest == expected.as_slice() {
                Ok(())
            } else {
                Err(WasmError::Load(
                    "integrity verification failed: digest mismatch".to_string(),
                ))
            }
        }
        other => Err(WasmError::Load(format!(
            "unsupported integrity scheme {other:#04x}"
        ))),
    }
}
