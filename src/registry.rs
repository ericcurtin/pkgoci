use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use indicatif::ProgressBar;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::{Method, Response, StatusCode};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::oci::{self, Descriptor, Index, Manifest};

pub struct Client {
    http: reqwest::Client,
    registry: String,
    tokens: tokio::sync::Mutex<HashMap<String, String>>,
}

/// A platform-resolved manifest plus the index it came from (if any).
pub struct Resolved {
    pub manifest: Manifest,
    pub manifest_digest: String,
    pub index: Option<Index>,
}

impl Resolved {
    /// Best-effort version: manifest annotation, then index annotation,
    /// then a short digest.
    pub fn version(&self, tag: &str) -> String {
        self.manifest
            .annotation(oci::ANNOTATION_VERSION)
            .or_else(|| {
                self.index
                    .as_ref()
                    .and_then(|i| i.annotation(oci::ANNOTATION_VERSION))
            })
            .map(str::to_string)
            .unwrap_or_else(|| {
                if tag != "latest" {
                    tag.to_string()
                } else {
                    self.manifest_digest.trim_start_matches("sha256:")[..12].to_string()
                }
            })
    }

    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.manifest
            .annotation(key)
            .or_else(|| self.index.as_ref().and_then(|i| i.annotation(key)))
    }
}

impl Client {
    pub fn new(registry: &str) -> Result<Self> {
        Ok(Client {
            http: reqwest::Client::builder()
                .user_agent(concat!("pkgoci/", env!("CARGO_PKG_VERSION")))
                .build()?,
            registry: registry.to_string(),
            tokens: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    fn scheme(&self) -> &'static str {
        // Local registries (e.g. `registry:2` for testing) speak plain HTTP.
        if self.registry.starts_with("localhost") || self.registry.starts_with("127.") {
            "http"
        } else {
            "https"
        }
    }

    fn url(&self, repo: &str, path: &str) -> String {
        format!("{}://{}/v2/{}/{}", self.scheme(), self.registry, repo, path)
    }

    /// Perform a v2 API request, transparently handling Bearer token
    /// challenges (anonymous pull, or PKGOCI_USERNAME/PKGOCI_PASSWORD for push).
    async fn request(
        &self,
        method: Method,
        repo: &str,
        path: &str,
        accept: Option<&str>,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        for attempt in 0..2 {
            let mut req = self.http.request(method.clone(), self.url(repo, path));
            if let Some(a) = accept {
                req = req.header(ACCEPT, a);
            }
            if let Some(ct) = content_type {
                req = req.header(CONTENT_TYPE, ct);
            }
            if let Some(b) = &body {
                req = req.body(b.clone());
            }
            if let Some(tok) = self.tokens.lock().await.get(repo) {
                req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
            }
            let resp = req.send().await?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let challenge = resp
                    .headers()
                    .get(WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("401 without WWW-Authenticate from {}", self.registry))?
                    .to_string();
                let token = self.fetch_token(&challenge).await?;
                self.tokens.lock().await.insert(repo.to_string(), token);
                continue;
            }
            return Ok(resp);
        }
        unreachable!()
    }

    async fn fetch_token(&self, challenge: &str) -> Result<String> {
        let params = parse_challenge(challenge);
        let realm = params
            .get("realm")
            .ok_or_else(|| anyhow!("no realm in auth challenge"))?;
        let mut req = self.http.get(realm);
        if let Some(service) = params.get("service") {
            req = req.query(&[("service", service.as_str())]);
        }
        if let Some(scope) = params.get("scope") {
            req = req.query(&[("scope", scope.as_str())]);
        }
        if let (Ok(user), Ok(pass)) = (
            std::env::var("PKGOCI_USERNAME"),
            std::env::var("PKGOCI_PASSWORD"),
        ) {
            req = req.basic_auth(user, Some(pass));
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .context("token request failed")?;
        let v: serde_json::Value = resp.json().await?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no token in auth response"))
    }

    /// Fetch `manifests/<reference>` and, if it is an index, descend into the
    /// manifest matching the host platform.
    pub async fn resolve(&self, repo: &str, reference: &str) -> Result<Resolved> {
        let resp = self
            .request(
                Method::GET,
                repo,
                &format!("manifests/{reference}"),
                Some(oci::ACCEPT_ANY_MANIFEST),
                None,
                None,
            )
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            bail!("package not found: {}/{repo}:{reference}", self.registry);
        }
        let resp = resp.error_for_status()?;
        let media_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await?;
        let digest = format!("sha256:{}", oci::sha256_hex(&bytes));

        if media_type == oci::MT_OCI_INDEX || media_type == oci::MT_DOCKER_LIST {
            let index: Index = serde_json::from_slice(&bytes)?;
            let (os, arch) = (crate::platform::os(), crate::platform::arch());
            let desc = index.select(os, arch).ok_or_else(|| {
                anyhow!(
                    "{repo}:{reference} has no build for {os}/{arch} (available: {})",
                    index.platforms().join(", ")
                )
            })?;
            let child_digest = desc.digest.clone();
            let resp = self
                .request(
                    Method::GET,
                    repo,
                    &format!("manifests/{child_digest}"),
                    Some(oci::ACCEPT_ANY_MANIFEST),
                    None,
                    None,
                )
                .await?
                .error_for_status()?;
            let bytes = resp.bytes().await?;
            let manifest: Manifest = serde_json::from_slice(&bytes)?;
            Ok(Resolved {
                manifest,
                manifest_digest: child_digest,
                index: Some(index),
            })
        } else {
            let manifest: Manifest = serde_json::from_slice(&bytes)?;
            Ok(Resolved {
                manifest,
                manifest_digest: digest,
                index: None,
            })
        }
    }

    /// Stream a blob to `dest`, verifying its sha256 digest.
    pub async fn download_blob(
        &self,
        repo: &str,
        desc: &Descriptor,
        dest: &Path,
        pb: &ProgressBar,
    ) -> Result<()> {
        let resp = self
            .request(
                Method::GET,
                repo,
                &format!("blobs/{}", desc.digest),
                None,
                None,
                None,
            )
            .await?
            .error_for_status()?;
        pb.set_length(desc.size);
        let tmp = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            pb.inc(chunk.len() as u64);
        }
        file.flush().await?;
        drop(file);
        let got = format!("sha256:{}", hex::encode(hasher.finalize()));
        if got != desc.digest {
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!(
                "digest mismatch for {}: expected {}, got {got}",
                dest.display(),
                desc.digest
            );
        }
        tokio::fs::rename(&tmp, dest).await?;
        Ok(())
    }

    /// Upload a blob if not already present. Returns without transfer when the
    /// registry already has the digest.
    pub async fn push_blob(&self, repo: &str, digest: &str, data: Vec<u8>) -> Result<()> {
        let head = self
            .request(
                Method::HEAD,
                repo,
                &format!("blobs/{digest}"),
                None,
                None,
                None,
            )
            .await?;
        if head.status().is_success() {
            return Ok(());
        }
        let resp = self
            .request(Method::POST, repo, "blobs/uploads/", None, None, None)
            .await?
            .error_for_status()
            .context("starting blob upload")?;
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("no upload location returned"))?;
        let mut url = if location.starts_with("http") {
            location.to_string()
        } else {
            format!("{}://{}{}", self.scheme(), self.registry, location)
        };
        url.push_str(if url.contains('?') { "&" } else { "?" });
        url.push_str(&format!("digest={digest}"));
        let token = self.tokens.lock().await.get(repo).cloned();
        let mut req = self
            .http
            .put(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data);
        if let Some(tok) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
        }
        req.send()
            .await?
            .error_for_status()
            .context("completing blob upload")?;
        Ok(())
    }

    pub async fn push_manifest(
        &self,
        repo: &str,
        reference: &str,
        media_type: &str,
        body: Vec<u8>,
    ) -> Result<()> {
        self.request(
            Method::PUT,
            repo,
            &format!("manifests/{reference}"),
            None,
            Some(media_type),
            Some(body),
        )
        .await?
        .error_for_status()
        .with_context(|| format!("pushing manifest {repo}:{reference}"))?;
        Ok(())
    }
}

fn parse_challenge(header: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let header = header
        .trim_start_matches("Bearer ")
        .trim_start_matches("bearer ");
    for part in header.split(',') {
        if let Some((k, v)) = part.trim().split_once('=') {
            out.insert(k.to_string(), v.trim_matches('"').to_string());
        }
    }
    out
}
