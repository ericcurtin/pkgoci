//! Rekor transparency log integration (sigstore).
//!
//! `pkgoci push --sign --rekor` uploads the package signature as a `rekord`
//! entry; the log's receipt (Signed Entry Timestamp and inclusion metadata)
//! is stored on the signature artifact so `pkgoci verify` can prove the
//! signature was publicly logged.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const DEFAULT_URL: &str = "https://rekor.sigstore.dev";

pub fn url() -> String {
    std::env::var("PKGOCI_REKOR_URL").unwrap_or_else(|_| DEFAULT_URL.into())
}

/// A logged entry's receipt, stored in the signature artifact annotation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub url: String,
    pub uuid: String,
    pub body: String,
    pub log_index: u64,
    pub integrated_time: i64,
    pub log_id: String,
    pub signed_entry_timestamp: String,
}

/// Upload a signature over `artifact` to the log. Returns the receipt.
pub fn upload(base: &str, artifact: &[u8], sig_b64: &str, public_key_pem: &str) -> Result<Entry> {
    let b64 = &base64::engine::general_purpose::STANDARD;
    let proposed = serde_json::json!({
        "apiVersion": "0.0.1",
        "kind": "rekord",
        "spec": {
            "data": {"content": b64.encode(artifact)},
            "signature": {
                "format": "x509",
                "content": sig_b64,
                "publicKey": {"content": b64.encode(public_key_pem)}
            }
        }
    });
    let endpoint = format!("{base}/api/v1/log/entries");
    let response = match ureq::post(&endpoint).send_json(&proposed) {
        Ok(resp) => resp,
        // 409: this exact entry is already in the log; fetch it.
        Err(ureq::Error::Status(409, resp)) => {
            let location = resp
                .header("location")
                .ok_or_else(|| anyhow!("rekor 409 without Location header"))?
                .to_string();
            ureq::get(&format!("{base}{location}"))
                .call()
                .context("fetching existing rekor entry")?
        }
        Err(e) => return Err(anyhow!("uploading to rekor at {endpoint}: {e}")),
    };
    let v: serde_json::Value = response.into_json()?;
    let (uuid, entry) = v
        .as_object()
        .and_then(|o| o.iter().next())
        .ok_or_else(|| anyhow!("empty rekor response"))?;
    Ok(Entry {
        url: base.to_string(),
        uuid: uuid.clone(),
        body: entry["body"].as_str().unwrap_or_default().to_string(),
        log_index: entry["logIndex"].as_u64().unwrap_or_default(),
        integrated_time: entry["integratedTime"].as_i64().unwrap_or_default(),
        log_id: entry["logID"].as_str().unwrap_or_default().to_string(),
        signed_entry_timestamp: entry["verification"]["signedEntryTimestamp"]
            .as_str()
            .ok_or_else(|| anyhow!("rekor response missing signedEntryTimestamp"))?
            .to_string(),
    })
}

/// Verify a log receipt: the entry must bind our signature and artifact, and
/// the Signed Entry Timestamp must verify against the log's public key.
pub fn verify(entry: &Entry, expected_sig_b64: &str, artifact_sha256_hex: &str) -> Result<()> {
    use p256::ecdsa::signature::Verifier;
    let b64 = &base64::engine::general_purpose::STANDARD;

    // 1. The logged body is about the signature we already verified.
    let body: serde_json::Value = serde_json::from_slice(
        &b64.decode(&entry.body)
            .context("decoding rekor entry body")?,
    )?;
    let logged_sig = body
        .pointer("/spec/signature/content")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    if logged_sig != expected_sig_b64 {
        bail!("rekor entry signature does not match the package signature");
    }
    let logged_hash = body
        .pointer("/spec/data/hash/value")
        .and_then(|h| h.as_str())
        .map(str::to_string)
        .or_else(|| {
            body.pointer("/spec/data/content")
                .and_then(|c| c.as_str())
                .and_then(|c| b64.decode(c).ok())
                .map(|data| crate::oci::sha256_hex(&data))
        })
        .unwrap_or_default();
    if logged_hash != artifact_sha256_hex {
        bail!("rekor entry artifact hash does not match the signed payload");
    }

    // 2. The Signed Entry Timestamp verifies against the log's key.
    // (Canonical JSON: serde_json sorts object keys.)
    use p256::pkcs8::DecodePublicKey;
    let canonical = serde_json::to_vec(&serde_json::json!({
        "body": entry.body,
        "integratedTime": entry.integrated_time,
        "logID": entry.log_id,
        "logIndex": entry.log_index,
    }))?;
    let pem = ureq::get(&format!("{}/api/v1/log/publicKey", entry.url))
        .call()
        .with_context(|| format!("fetching rekor public key from {}", entry.url))?
        .into_string()?;
    let key = p256::ecdsa::VerifyingKey::from_public_key_pem(&pem)
        .map_err(|e| anyhow!("parsing rekor public key: {e}"))?;
    let set = b64
        .decode(&entry.signed_entry_timestamp)
        .context("decoding signed entry timestamp")?;
    let sig = p256::ecdsa::Signature::from_der(&set)
        .map_err(|e| anyhow!("parsing signed entry timestamp: {e}"))?;
    key.verify(&canonical, &sig)
        .map_err(|_| anyhow!("signed entry timestamp verification failed"))?;
    Ok(())
}
