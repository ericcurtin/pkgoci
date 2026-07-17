use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MT_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MT_LAYER_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MT_LAYER_TAR_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const MT_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Accept header covering everything a `manifests/<ref>` GET may return.
pub const ACCEPT_ANY_MANIFEST: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json";

pub const ANNOTATION_VERSION: &str = "org.opencontainers.image.version";
pub const ANNOTATION_DESCRIPTION: &str = "org.opencontainers.image.description";
pub const ANNOTATION_URL: &str = "org.opencontainers.image.url";
pub const ANNOTATION_LICENSES: &str = "org.opencontainers.image.licenses";

pub type Annotations = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
}

impl Manifest {
    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations
            .as_ref()
            .and_then(|a| a.get(key))
            .map(String::as_str)
    }
}

impl Index {
    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations
            .as_ref()
            .and_then(|a| a.get(key))
            .map(String::as_str)
    }

    /// Select the manifest descriptor matching `os`/`arch`.
    pub fn select(&self, os: &str, arch: &str) -> Option<&Descriptor> {
        self.manifests.iter().find(|d| {
            d.platform
                .as_ref()
                .is_some_and(|p| p.os == os && p.architecture == arch)
        })
    }

    pub fn platforms(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .manifests
            .iter()
            .filter_map(|d| d.platform.as_ref())
            .filter(|p| p.os != "unknown") // skip attestation manifests
            .map(|p| format!("{}/{}", p.os, p.architecture))
            .collect();
        out.dedup();
        out
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}
