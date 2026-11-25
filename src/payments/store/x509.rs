//! Certificate chain validation for App Store signed payloads.
//!
//! Apple signs transactions and notifications with a leaf certificate whose
//! chain (`x5c` header: leaf, intermediate, root) ends in a pinned root. This
//! is the narrow validation Apple documents: the root must be byte-for-byte a
//! trusted root, each certificate must be signed by the next, all must be
//! valid at the payload's signing time, and the leaf and intermediate must
//! carry Apple's marker extensions. Revocation (OCSP) is not checked.
use super::jose::Checked;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA384_ASN1, ECDSA_P384_SHA256_ASN1, ECDSA_P384_SHA384_ASN1,
    UnparsedPublicKey, VerificationAlgorithm,
};
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

/// Marker OID of Apple's leaf certificates for the App Store receipt signing.
const LEAF_MARKER: &str = "1.2.840.113635.100.6.11.1";
/// Marker OID of the Apple Worldwide Developer Relations intermediate.
const INTERMEDIATE_MARKER: &str = "1.2.840.113635.100.6.2.1";
/// `ecdsa-with-SHA256`.
const ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
/// `ecdsa-with-SHA384`.
const ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
/// Length of an uncompressed P-256 public point.
const P256_POINT: usize = 65;
/// Length of an uncompressed P-384 public point.
const P384_POINT: usize = 97;

/// Validates an `x5c` chain `[leaf, intermediate, root]` (DER) against
/// `roots` at Unix time `at` and returns the leaf's P-256 public key.
pub fn verify_chain(chain: &[Vec<u8>], roots: &[Vec<u8>], at: i64) -> Checked<Vec<u8>> {
    let [leaf, intermediate, root] = chain else {
        return Err("expected a three certificate chain");
    };
    if !roots.iter().any(|trusted| trusted == root) {
        return Err("untrusted root certificate");
    }
    let leaf = parse(leaf)?;
    let intermediate = parse(intermediate)?;
    let root = parse(root)?;
    for cert in [&leaf, &intermediate, &root] {
        let valid = cert.validity();
        if at < valid.not_before.timestamp() || at > valid.not_after.timestamp() {
            return Err("certificate not valid at signing time");
        }
    }
    check_signed_by(&intermediate, &root)?;
    check_signed_by(&leaf, &intermediate)?;
    check_signed_by(&root, &root)?;
    if !is_ca(&intermediate) || !is_ca(&root) || is_ca(&leaf) {
        return Err("unexpected certificate roles");
    }
    if !has_extension(&intermediate, INTERMEDIATE_MARKER) {
        return Err("intermediate lacks the Apple marker");
    }
    if !has_extension(&leaf, LEAF_MARKER) {
        return Err("leaf lacks the Apple marker");
    }
    let key = leaf.public_key().subject_public_key.as_ref();
    if key.len() != P256_POINT {
        return Err("leaf key is not P-256");
    }
    Ok(key.to_vec())
}

/// Parses one DER certificate, rejecting trailing bytes.
fn parse(der: &[u8]) -> Checked<X509Certificate<'_>> {
    match X509Certificate::from_der(der) {
        Ok(([], cert)) => Ok(cert),
        _ => Err("invalid certificate"),
    }
}

/// Whether the certificate asserts `CA:TRUE`.
fn is_ca(cert: &X509Certificate<'_>) -> bool {
    matches!(cert.basic_constraints(), Ok(Some(bc)) if bc.value.ca)
}

/// Whether the certificate carries the extension with this OID.
fn has_extension(cert: &X509Certificate<'_>, oid: &str) -> bool {
    cert.extensions()
        .iter()
        .any(|ext| ext.oid.to_id_string() == oid)
}

/// Verifies that `issuer` signed `cert`, choosing the ECDSA algorithm from
/// the issuer's curve and the certificate's signature hash.
fn check_signed_by(cert: &X509Certificate<'_>, issuer: &X509Certificate<'_>) -> Checked<()> {
    if cert.issuer().as_raw() != issuer.subject().as_raw() {
        return Err("issuer name mismatch");
    }
    let key = issuer.public_key().subject_public_key.as_ref();
    let hash = cert.signature_algorithm.algorithm.to_id_string();
    let algorithm: &'static dyn VerificationAlgorithm = match (key.len(), hash.as_str()) {
        (P256_POINT, ECDSA_SHA256) => &ECDSA_P256_SHA256_ASN1,
        (P256_POINT, ECDSA_SHA384) => &ECDSA_P256_SHA384_ASN1,
        (P384_POINT, ECDSA_SHA256) => &ECDSA_P384_SHA256_ASN1,
        (P384_POINT, ECDSA_SHA384) => &ECDSA_P384_SHA384_ASN1,
        _ => return Err("unsupported certificate signature"),
    };
    UnparsedPublicKey::new(algorithm, key)
        .verify(cert.tbs_certificate.as_ref(), cert.signature_value.as_ref())
        .map_err(|_| "certificate signature invalid")
}

/// A generated Apple-shaped chain for tests.
#[cfg(test)]
pub mod testing {
    use rcgen::{
        BasicConstraints, Certificate, CertificateParams, CustomExtension, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384,
    };

    /// A root, intermediate, and leaf, plus the leaf's PKCS#8 key.
    pub struct Chain {
        pub der: Vec<Vec<u8>>,
        pub leaf_key: Vec<u8>,
    }

    /// Builds a chain, optionally leaving out the Apple marker extensions.
    pub fn chain(markers: bool) -> Chain {
        let marker = |oid: &[u64]| CustomExtension::from_oid_content(oid, vec![0x05, 0x00]);
        let root_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test Root");
        let root = params.self_signed(&root_key).unwrap();
        let root_issuer = rcgen::Issuer::new(params, root_key);

        let mid_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test Intermediate");
        if markers {
            params
                .custom_extensions
                .push(marker(&[1, 2, 840, 113635, 100, 6, 2, 1]));
        }
        let mid = params.signed_by(&mid_key, &root_issuer).unwrap();
        let mid_issuer = rcgen::Issuer::new(params, mid_key);

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test Leaf");
        if markers {
            params
                .custom_extensions
                .push(marker(&[1, 2, 840, 113635, 100, 6, 11, 1]));
        }
        let leaf = params.signed_by(&leaf_key, &mid_issuer).unwrap();
        let der = |c: &Certificate| c.der().to_vec();
        Chain {
            der: vec![der(&leaf), der(&mid), der(&root)],
            leaf_key: leaf_key.serialize_der(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::chain, *};

    fn now() -> i64 {
        crate::db::now()
    }

    #[test]
    fn accepts_a_chain_to_a_trusted_root() {
        let c = chain(true);
        let key = verify_chain(&c.der, &[c.der[2].clone()], now()).unwrap();
        assert_eq!(key, crate::payments::store::jose::es256_public(&c.leaf_key));
    }

    #[test]
    fn rejects_an_untrusted_root() {
        let c = chain(true);
        let other = chain(true);
        assert_eq!(
            verify_chain(&c.der, &[other.der[2].clone()], now()),
            Err("untrusted root certificate")
        );
        assert!(verify_chain(&c.der, &[], now()).is_err());
    }

    #[test]
    fn rejects_missing_markers() {
        let c = chain(false);
        assert_eq!(
            verify_chain(&c.der, &[c.der[2].clone()], now()),
            Err("intermediate lacks the Apple marker")
        );
    }

    #[test]
    fn rejects_a_spliced_chain_and_wrong_shapes() {
        let a = chain(true);
        let b = chain(true);
        // A leaf from another chain is not signed by this intermediate.
        let spliced = vec![b.der[0].clone(), a.der[1].clone(), a.der[2].clone()];
        assert!(verify_chain(&spliced, &[a.der[2].clone()], now()).is_err());
        assert!(verify_chain(&a.der[..2], &[a.der[2].clone()], now()).is_err());
        let mut junk = a.der.clone();
        junk[0].push(0);
        assert!(verify_chain(&junk, &[a.der[2].clone()], now()).is_err());
    }

    #[test]
    fn rejects_signing_times_outside_validity() {
        let c = chain(true);
        let roots = [c.der[2].clone()];
        assert!(verify_chain(&c.der, &roots, 0).is_err());
        assert!(verify_chain(&c.der, &roots, i64::MAX / 2).is_err());
    }

    #[test]
    fn pinned_apple_root_parses_with_the_documented_fingerprint() {
        use sha2::{Digest, Sha256};
        let der = crate::payments::store::apple::ROOT_CA_G3;
        let digest = hex::encode(Sha256::digest(der));
        assert_eq!(
            digest,
            "63343abfb89a6a03ebb57e9b3f5fa7be7c4f5c756f3017b3a8c488c3653e9179"
        );
        let cert = parse(der).unwrap();
        assert!(is_ca(&cert));
        check_signed_by(&cert, &cert).unwrap();
    }
}
