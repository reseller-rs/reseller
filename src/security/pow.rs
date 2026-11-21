//! ALTCHA-compatible (v1, SHA-256) proof-of-work challenges for anonymous
//! signup.
//!
//! A challenge is `sha256(salt + number)` for a random `number` in
//! `[0, maxnumber]`; the client finds the number by brute force. The salt
//! carries its own expiry (`<hex>?expires=<unix>`) and the server signs the
//! challenge with HMAC-SHA256 under a key derived from the pepper, so the
//! server stays stateless until a solution is redeemed (single use, recorded
//! in `pow_spent` by the caller).
use crate::error::{Error, Result};
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const ALGORITHM: &str = "SHA-256";
/// Domain-separation label for the signing key.
const KEY_LABEL: &[u8] = b"reseller/signup-pow/v1";
/// Largest accepted base64 solution payload.
const MAX_PAYLOAD_BYTES: usize = 2048;

/// A challenge as served to clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    pub algorithm: String,
    pub challenge: String,
    pub maxnumber: u64,
    pub salt: String,
    pub signature: String,
}

/// A client's answer (the base64-encoded JSON `altcha` field).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    pub algorithm: String,
    pub challenge: String,
    pub number: u64,
    pub salt: String,
    pub signature: String,
}

/// A verified, not yet spent solution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Identifier to record for single use.
    pub challenge: String,
    /// Unix time after which the solution can no longer be replayed (and the
    /// spent record may be purged).
    pub expires: i64,
}

fn signing_key(pepper: &str) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC accepts any key length");
    mac.update(KEY_LABEL);
    mac.finalize().into_bytes().into()
}

fn sign(pepper: &str, challenge: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&signing_key(pepper)).expect("HMAC accepts any key length");
    mac.update(challenge.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn digest(salt: &str, number: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(number.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Issues a challenge that expires `ttl_seconds` after `now`.
pub fn create(pepper: &str, max_number: u32, ttl_seconds: u32, now: i64) -> Challenge {
    let mut random = [0u8; 20];
    getrandom::fill(&mut random).expect("operating system random source is available");
    let salt = format!(
        "{}?expires={}",
        hex::encode(&random[..12]),
        now + i64::from(ttl_seconds)
    );
    // Rejection-free enough: the modulo bias over 2^64 is negligible.
    let number =
        u64::from_le_bytes(random[12..].try_into().expect("8 bytes")) % (u64::from(max_number) + 1);
    let challenge = digest(&salt, number);
    Challenge {
        algorithm: ALGORITHM.into(),
        signature: sign(pepper, &challenge),
        challenge,
        maxnumber: u64::from(max_number),
        salt,
    }
}

/// Brute-forces the challenge (used by clients such as the stdio MCP bridge).
pub fn solve(challenge: &Challenge) -> Option<u64> {
    (0..=challenge.maxnumber).find(|n| digest(&challenge.salt, *n) == challenge.challenge)
}

/// Encodes a solution as the base64 `altcha` payload.
pub fn encode(challenge: &Challenge, number: u64) -> String {
    let solution = Solution {
        algorithm: challenge.algorithm.clone(),
        challenge: challenge.challenge.clone(),
        number,
        salt: challenge.salt.clone(),
        signature: challenge.signature.clone(),
    };
    STANDARD.encode(serde_json::to_vec(&solution).expect("solution is serializable"))
}

/// `expires` Unix time from the salt's query string.
fn expires(salt: &str) -> Option<i64> {
    let (_, query) = salt.split_once('?')?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "expires")
        .and_then(|(_, v)| v.parse().ok())
}

fn invalid(message: &str) -> Error {
    Error::new(StatusCode::BAD_REQUEST, "pow_invalid", message)
}

/// Verifies an `altcha` payload against the signing key, the current
/// difficulty `max_number`, and the clock. Does not check for replays.
pub fn verify(pepper: &str, payload: &str, max_number: u32, now: i64) -> Result<Verified> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid("Proof of work payload is too large"));
    }
    let solution: Solution = STANDARD
        .decode(payload.trim())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(|| invalid("Proof of work payload is malformed"))?;
    if solution.algorithm != ALGORITHM {
        return Err(invalid("Unsupported proof of work algorithm"));
    }
    // Authenticity first: nothing else in the payload is trusted before this.
    if !equal(&sign(pepper, &solution.challenge), &solution.signature) {
        return Err(invalid("Proof of work signature is invalid"));
    }
    let expires = expires(&solution.salt).ok_or_else(|| invalid("Challenge has no expiry"))?;
    if expires <= now {
        return Err(invalid("Challenge expired; request a new one"));
    }
    if solution.number > u64::from(max_number) {
        return Err(invalid("Proof of work number is out of range"));
    }
    if !equal(
        &digest(&solution.salt, solution.number),
        &solution.challenge,
    ) {
        return Err(invalid("Proof of work solution is incorrect"));
    }
    Ok(Verified {
        challenge: solution.challenge,
        expires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEPPER: &str = "pepper-test-secret-12345678";
    const NOW: i64 = 1_700_000_000;

    fn code(e: &Error) -> &'static str {
        e.code
    }

    #[test]
    fn solve_and_verify_round_trip() {
        let c = create(PEPPER, 500, 300, NOW);
        assert_eq!(c.algorithm, "SHA-256");
        assert_eq!(c.maxnumber, 500);
        assert!(c.salt.ends_with(&format!("?expires={}", NOW + 300)));
        let n = solve(&c).expect("solvable");
        assert!(n <= 500);
        let ok = verify(PEPPER, &encode(&c, n), 500, NOW).unwrap();
        assert_eq!(ok.challenge, c.challenge);
        assert_eq!(ok.expires, NOW + 300);
    }

    #[test]
    fn zero_difficulty_still_round_trips() {
        let c = create(PEPPER, 0, 60, NOW);
        assert_eq!(solve(&c), Some(0));
    }

    #[test]
    fn rejects_wrong_number_signature_key_and_tampering() {
        let c = create(PEPPER, 500, 300, NOW);
        let n = solve(&c).unwrap();
        let wrong = (n + 1) % 501;
        assert_eq!(
            code(&verify(PEPPER, &encode(&c, wrong), 500, NOW).unwrap_err()),
            "pow_invalid"
        );
        // A different pepper cannot verify.
        assert!(verify("another-pepper-123456789012", &encode(&c, n), 500, NOW).is_err());
        // Forged signature.
        let mut forged = c.clone();
        forged.signature = "0".repeat(64);
        assert!(verify(PEPPER, &encode(&forged, n), 500, NOW).is_err());
        // Extending the expiry in the salt breaks the hash and the signature.
        let mut longer = c.clone();
        longer.salt = longer
            .salt
            .replace(&(NOW + 300).to_string(), &(NOW + 9999).to_string());
        assert!(verify(PEPPER, &encode(&longer, n), 500, NOW).is_err());
        // A self-made challenge without the server's signature.
        let own_salt = format!("aa?expires={}", NOW + 300);
        let own = Challenge {
            algorithm: ALGORITHM.into(),
            challenge: digest(&own_salt, 0),
            maxnumber: 0,
            salt: own_salt,
            signature: sign("guess", "x"),
        };
        assert!(verify(PEPPER, &encode(&own, 0), 500, NOW).is_err());
    }

    #[test]
    fn rejects_expired_wrong_algorithm_oversized_number_and_garbage() {
        let c = create(PEPPER, 500, 300, NOW);
        let n = solve(&c).unwrap();
        let payload = encode(&c, n);
        let late = verify(PEPPER, &payload, 500, NOW + 300).unwrap_err();
        assert!(late.message.contains("expired"), "{late}");
        let mut other = c.clone();
        other.algorithm = "SHA-1".into();
        let e = verify(PEPPER, &encode(&other, n), 500, NOW).unwrap_err();
        assert!(e.message.contains("algorithm"), "{e}");
        // The number must lie within the currently configured difficulty.
        if n > 0 {
            let e = verify(PEPPER, &payload, n as u32 - 1, NOW).unwrap_err();
            assert!(e.message.contains("range"), "{e}");
        }
        for junk in ["", "!!!", &STANDARD.encode("{}"), &"A".repeat(5000)] {
            assert_eq!(
                code(&verify(PEPPER, junk, 500, NOW).unwrap_err()),
                "pow_invalid"
            );
        }
    }
}
