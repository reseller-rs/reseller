//! Minimal JOSE helpers for the store integrations: base64url, compact JWS
//! splitting, and ES256 / RS256 signing and verification.
//!
//! Only what the App Store and Google Play flows need. Verification never
//! trusts the token's own `alg` beyond selecting between the two fixed
//! algorithms each caller allows.
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair,
        RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents,
        UnparsedPublicKey,
    },
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::Value;

/// Result of a check that fails with a short reason for the log.
pub type Checked<T> = std::result::Result<T, &'static str>;

/// Largest token accepted, in bytes.
const MAX_TOKEN: usize = 64 * 1024;

/// Encodes `data` as unpadded base64url.
pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Decodes unpadded base64url.
pub fn from_b64url(text: &str) -> Checked<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| "invalid base64url")
}

/// Decodes standard padded base64 (certificate chains, Pub/Sub data).
pub fn from_b64(text: &str) -> Checked<Vec<u8>> {
    STANDARD.decode(text).map_err(|_| "invalid base64")
}

/// Extracts the DER bytes of the first PEM block.
pub fn pem_to_der(pem: &str) -> Checked<Vec<u8>> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    from_b64(&body)
}

/// A compact JWS split into its decoded parts. The signature is not yet
/// verified.
#[derive(Debug)]
pub struct Jws<'a> {
    pub header: Value,
    pub payload: Value,
    /// `header.payload` exactly as transmitted: the signed bytes.
    pub signing_input: &'a str,
    pub signature: Vec<u8>,
}

/// Splits and decodes a compact JWS (three dot-separated parts).
pub fn split(token: &str) -> Checked<Jws<'_>> {
    if token.len() > MAX_TOKEN {
        return Err("token too large");
    }
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a compact JWS");
    };
    let json = |part: &str| -> Checked<Value> {
        serde_json::from_slice(&from_b64url(part)?).map_err(|_| "invalid JSON")
    };
    Ok(Jws {
        header: json(header)?,
        payload: json(payload)?,
        signing_input: &token[..header.len() + 1 + payload.len()],
        signature: from_b64url(signature)?,
    })
}

/// Joins `header.claims` and appends the base64url signature.
fn assemble(
    header: &Value,
    claims: &Value,
    sign: impl FnOnce(&[u8]) -> Checked<Vec<u8>>,
) -> Checked<String> {
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let signature = sign(input.as_bytes())?;
    Ok(format!("{input}.{}", b64url(&signature)))
}

/// Signs a JWT with an ES256 (P-256) PKCS#8 key. The signature is the raw
/// `r || s` form JOSE uses.
pub fn sign_es256(pkcs8: &[u8], header: &Value, claims: &Value) -> Checked<String> {
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8)
        .map_err(|_| "invalid ES256 key")?;
    assemble(header, claims, |input| {
        key.sign(&SystemRandom::new(), input)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| "signing failed")
    })
}

/// Signs a JWT with an RS256 PKCS#8 key.
pub fn sign_rs256(pkcs8: &[u8], header: &Value, claims: &Value) -> Checked<String> {
    let key = RsaKeyPair::from_pkcs8(pkcs8).map_err(|_| "invalid RS256 key")?;
    assemble(header, claims, |input| {
        let mut signature = vec![0; key.public_modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            input,
            &mut signature,
        )
        .map_err(|_| "signing failed")?;
        Ok(signature)
    })
}

/// Verifies an ES256 signature against an uncompressed P-256 public key.
pub fn verify_es256(public_key: &[u8], jws: &Jws<'_>) -> Checked<()> {
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public_key)
        .verify(jws.signing_input.as_bytes(), &jws.signature)
        .map_err(|_| "bad signature")
}

/// Verifies an RS256 signature against the big-endian modulus and exponent.
pub fn verify_rs256(modulus: &[u8], exponent: &[u8], jws: &Jws<'_>) -> Checked<()> {
    RsaPublicKeyComponents {
        n: modulus,
        e: exponent,
    }
    .verify(
        &RSA_PKCS1_2048_8192_SHA256,
        jws.signing_input.as_bytes(),
        &jws.signature,
    )
    .map_err(|_| "bad signature")
}

/// The public key (uncompressed point) of an ES256 PKCS#8 key.
#[cfg(test)]
pub fn es256_public(pkcs8: &[u8]) -> Vec<u8> {
    use aws_lc_rs::signature::KeyPair;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8).unwrap();
    key.public_key().as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{
        encoding::AsDer,
        rsa::{KeyPair as Rsa, KeySize},
        signature::KeyPair,
    };
    use serde_json::json;

    fn es_key() -> Vec<u8> {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .unwrap()
            .as_ref()
            .to_vec()
    }

    #[test]
    fn es256_round_trips_and_detects_tampering() {
        let pkcs8 = es_key();
        let token = sign_es256(&pkcs8, &json!({"alg": "ES256"}), &json!({"a": 1})).unwrap();
        let jws = split(&token).unwrap();
        assert_eq!(jws.payload["a"], 1);
        verify_es256(&es256_public(&pkcs8), &jws).unwrap();
        let (head, rest) = token.split_once('.').unwrap();
        let forged = format!(
            "{head}.{}.{}",
            b64url(br#"{"a":2}"#),
            rest.split_once('.').unwrap().1
        );
        let jws = split(&forged).unwrap();
        assert!(verify_es256(&es256_public(&pkcs8), &jws).is_err());
    }

    #[test]
    fn rs256_round_trips_and_rejects_other_keys() {
        let key = Rsa::generate(KeySize::Rsa2048).unwrap();
        let other = Rsa::generate(KeySize::Rsa2048).unwrap();
        let der = key.as_der().unwrap();
        let token = sign_rs256(der.as_ref(), &json!({"alg": "RS256"}), &json!({"b": 2})).unwrap();
        let jws = split(&token).unwrap();
        let parts = |k: &Rsa| {
            let c = aws_lc_rs::rsa::PublicKeyComponents::<Vec<u8>>::from(k.public_key());
            (c.n, c.e)
        };
        let (n, e) = parts(&key);
        verify_rs256(&n, &e, &jws).unwrap();
        let (n, e) = parts(&other);
        assert!(verify_rs256(&n, &e, &jws).is_err());
    }

    #[test]
    fn split_rejects_malformed_tokens() {
        for bad in ["", "a.b", "a.b.c.d", "!.!.!", "e30.e30"] {
            assert!(split(bad).is_err(), "{bad}");
        }
        let ok = format!("{0}.{0}.{1}", b64url(b"{}"), b64url(b"sig"));
        assert_eq!(split(&ok).unwrap().signature, b"sig");
        assert!(split(&"a".repeat(MAX_TOKEN + 1)).is_err());
    }

    #[test]
    fn pem_bodies_decode_across_lines() {
        let pem = format!(
            "-----BEGIN X-----\n{}\n{}\n-----END X-----\n",
            "AAAA", "BBBB"
        );
        assert_eq!(pem_to_der(&pem).unwrap(), from_b64("AAAABBBB").unwrap());
        assert!(pem_to_der("-----BEGIN X-----\n!!\n-----END X-----").is_err());
    }
}
