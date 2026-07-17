//! Cosign-compatible package signing.
//!
//! Signatures use the sigstore "simple signing" payload and cosign's storage
//! convention: an OCI artifact tagged `sha256-<digest>.sig` whose layer is
//! the payload blob, carrying the signature in the
//! `dev.cosignproject.cosign/signature` annotation. Keys are ed25519 in
//! standard PEM (PKCS#8 private / SPKI public), so packages signed by
//! `pkgoci push --sign` also verify with stock cosign:
//!
//! ```sh
//! cosign verify --key pkgoci.pub --insecure-ignore-tlog=true <ref>
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub const MT_SIMPLE_SIGNING: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
pub const MT_DSSE_ENVELOPE: &str = "application/vnd.dsse.envelope.v1+json";
pub const PAYLOAD_TYPE_IN_TOTO: &str = "application/vnd.in-toto+json";
pub const ANNOTATION_SIGNATURE: &str = "dev.cosignproject.cosign/signature";

/// Tag under which the signature for `root_digest` is stored.
pub fn sig_tag(root_digest: &str) -> String {
    format!("sha256-{}.sig", root_digest.trim_start_matches("sha256:"))
}

/// Tag under which attestations for `root_digest` are stored
/// (cosign's convention).
pub fn att_tag(root_digest: &str) -> String {
    format!("sha256-{}.att", root_digest.trim_start_matches("sha256:"))
}

/// The sigstore simple-signing payload for `root_digest` of `image_ref`,
/// serialized exactly as signed.
pub fn payload(image_ref: &str, root_digest: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "critical": {
            "identity": { "docker-reference": image_ref },
            "image": { "docker-manifest-digest": root_digest },
            "type": "cosign container image signature"
        },
        "optional": null
    }))
    .expect("payload serialization cannot fail")
}

pub fn generate(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let key_path = dir.join("pkgoci.key");
    let pub_path = dir.join("pkgoci.pub");
    if key_path.exists() {
        bail!("refusing to overwrite existing key: {}", key_path.display());
    }
    let pem = key
        .to_pkcs8_pem(Default::default())
        .map_err(|e| anyhow!("encoding private key: {e}"))?;
    std::fs::write(&key_path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let pub_pem = key
        .verifying_key()
        .to_public_key_pem(Default::default())
        .map_err(|e| anyhow!("encoding public key: {e}"))?;
    std::fs::write(&pub_path, pub_pem)?;
    Ok((key_path, pub_path))
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let pem = std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading signing key {} (run `pkgoci keygen`)",
            path.display()
        )
    })?;
    SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow!("parsing signing key {}: {e}", path.display()))
}

fn load_verifying_key(path: &Path) -> Result<VerifyingKey> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("reading public key {}", path.display()))?;
    VerifyingKey::from_public_key_pem(&pem)
        .map_err(|e| anyhow!("parsing public key {}: {e}", path.display()))
}

/// Expand a trust root (a `.pub` file, or a directory of them) into keys.
pub fn load_trusted_keys(root: &Path) -> Result<Vec<(PathBuf, VerifyingKey)>> {
    let mut keys = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "pub") {
                keys.push((path.clone(), load_verifying_key(&path)?));
            }
        }
        if keys.is_empty() {
            bail!("no .pub keys found in {}", root.display());
        }
    } else {
        keys.push((root.to_path_buf(), load_verifying_key(root)?));
    }
    Ok(keys)
}

/// Sign `payload_bytes`, returning the base64 signature for the cosign
/// annotation.
pub fn sign(key_path: &Path, payload_bytes: &[u8]) -> Result<String> {
    let key = load_signing_key(key_path)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(key.sign(payload_bytes).to_bytes()))
}

/// DSSE pre-authentication encoding: what is actually signed for envelopes.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

/// Wrap an in-toto statement in a signed DSSE envelope (cosign-compatible).
pub fn dsse_envelope(key_path: &Path, statement: &[u8]) -> Result<Vec<u8>> {
    let key = load_signing_key(key_path)?;
    let b64 = &base64::engine::general_purpose::STANDARD;
    let sig = key.sign(&pae(PAYLOAD_TYPE_IN_TOTO, statement));
    Ok(serde_json::to_vec(&serde_json::json!({
        "payloadType": PAYLOAD_TYPE_IN_TOTO,
        "payload": b64.encode(statement),
        "signatures": [{"keyid": "", "sig": b64.encode(sig.to_bytes())}]
    }))?)
}

/// Verify a DSSE envelope against the trusted keys; returns the decoded
/// in-toto statement and the matching key path.
pub fn verify_dsse(
    trusted: &[(PathBuf, VerifyingKey)],
    envelope_bytes: &[u8],
) -> Result<(serde_json::Value, PathBuf)> {
    let b64 = &base64::engine::general_purpose::STANDARD;
    let envelope: serde_json::Value = serde_json::from_slice(envelope_bytes)?;
    let payload_type = envelope["payloadType"].as_str().unwrap_or_default();
    let payload = b64
        .decode(envelope["payload"].as_str().unwrap_or_default())
        .map_err(|_| anyhow!("malformed DSSE payload"))?;
    let message = pae(payload_type, &payload);
    for sig_entry in envelope["signatures"].as_array().into_iter().flatten() {
        let Some(sig_b64) = sig_entry["sig"].as_str() else {
            continue;
        };
        if let Ok(key) = verify(trusted, &message, sig_b64) {
            return Ok((serde_json::from_slice(&payload)?, key));
        }
    }
    bail!("attestation verification failed (no trusted key matches)")
}

/// Verify a base64 cosign signature over `payload_bytes` against any trusted
/// key. Returns the path of the key that matched.
pub fn verify(
    trusted: &[(PathBuf, VerifyingKey)],
    payload_bytes: &[u8],
    signature_b64: &str,
) -> Result<PathBuf> {
    let sig_bytes: [u8; 64] = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| anyhow!("malformed signature"))?;
    let sig = Signature::from_bytes(&sig_bytes);
    for (path, key) in trusted {
        if key.verify(payload_bytes, &sig).is_ok() {
            return Ok(path.clone());
        }
    }
    bail!("signature verification failed (no trusted key matches, or artifact was tampered with)")
}
