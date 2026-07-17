//! Ed25519 package signing. Signatures are made over the root (tag-level)
//! artifact digest and stored in the registry as an OCI artifact tagged
//! `sha256-<digest>.sig` (the cosign "triangle" convention).

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The payload stored as the signature artifact's layer blob.
#[derive(Serialize, Deserialize)]
pub struct SignaturePayload {
    /// Digest that was signed, e.g. `sha256:...` of the image index.
    pub digest: String,
    /// Hex ed25519 signature over the digest string's bytes.
    pub signature: String,
    /// Hex ed25519 public key (informational; trust comes from the
    /// verifier's own key file).
    pub public_key: String,
}

/// Tag under which the signature for `root_digest` is stored.
pub fn sig_tag(root_digest: &str) -> String {
    format!("sha256-{}.sig", root_digest.trim_start_matches("sha256:"))
}

pub fn generate(dir: &Path) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let key_path = dir.join("pkgoci.key");
    let pub_path = dir.join("pkgoci.pub");
    if key_path.exists() {
        bail!("refusing to overwrite existing key: {}", key_path.display());
    }
    std::fs::write(&key_path, hex::encode(key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::write(&pub_path, hex::encode(key.verifying_key().to_bytes()))?;
    Ok((key_path, pub_path))
}

fn read_hex_key(path: &Path, len: usize) -> Result<Vec<u8>> {
    let hex_str =
        std::fs::read_to_string(path).with_context(|| format!("reading key {}", path.display()))?;
    let bytes =
        hex::decode(hex_str.trim()).with_context(|| format!("decoding key {}", path.display()))?;
    if bytes.len() != len {
        bail!("{} is not a {len}-byte hex key", path.display());
    }
    Ok(bytes)
}

pub fn sign(key_path: &Path, root_digest: &str) -> Result<SignaturePayload> {
    let bytes: [u8; 32] = read_hex_key(key_path, 32)?.try_into().unwrap();
    let key = SigningKey::from_bytes(&bytes);
    Ok(SignaturePayload {
        digest: root_digest.to_string(),
        signature: hex::encode(key.sign(root_digest.as_bytes()).to_bytes()),
        public_key: hex::encode(key.verifying_key().to_bytes()),
    })
}

pub fn verify(pub_path: &Path, root_digest: &str, payload: &SignaturePayload) -> Result<()> {
    if payload.digest != root_digest {
        bail!("signature is for {}, not {root_digest}", payload.digest);
    }
    let bytes: [u8; 32] = read_hex_key(pub_path, 32)?.try_into().unwrap();
    let key = VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("invalid public key: {e}"))?;
    let sig_bytes: [u8; 64] = hex::decode(&payload.signature)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| anyhow!("malformed signature"))?;
    key.verify(root_digest.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow!("signature verification failed (wrong key or tampered artifact)"))
}
