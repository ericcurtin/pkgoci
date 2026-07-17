//! Registry access via containerd's distribution stack
//! (core/remotes/docker), linked in as a Go c-archive. See go/main.go.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use anyhow::{anyhow, bail, Result};

use crate::oci::{self, Descriptor, Index, Manifest};

extern "C" {
    fn PkgociResolve(
        reference: *const c_char,
        os: *const c_char,
        arch: *const c_char,
    ) -> *mut c_char;
    fn PkgociFetchBlob(
        reference: *const c_char,
        digest: *const c_char,
        media_type: *const c_char,
        size: i64,
        dest: *const c_char,
    ) -> *mut c_char;
    fn PkgociPushBlob(
        reference: *const c_char,
        digest: *const c_char,
        size: i64,
        path: *const c_char,
    ) -> *mut c_char;
    fn PkgociPushManifest(
        reference: *const c_char,
        media_type: *const c_char,
        body: *const c_char,
    ) -> *mut c_char;
    fn PkgociFree(p: *mut c_char);
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("string contains NUL byte"))
}

/// Take ownership of a JSON result string returned by the Go side.
fn take_result(ptr: *mut c_char) -> Result<serde_json::Value> {
    assert!(!ptr.is_null());
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { PkgociFree(ptr) };
    let v: serde_json::Value = serde_json::from_str(&s)?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        bail!("{e}");
    }
    Ok(v)
}

pub struct Client {
    registry: String,
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
    pub fn new(registry: &str) -> Self {
        Client {
            registry: registry.to_string(),
        }
    }

    /// Full containerd reference, e.g. `registry-1.docker.io/pkgoci/jq:latest`
    /// or `.../jq@sha256:...` when given a digest.
    fn reference(&self, repo: &str, reference: &str) -> String {
        if reference.starts_with("sha256:") {
            format!("{}/{repo}@{reference}", self.registry)
        } else {
            format!("{}/{repo}:{reference}", self.registry)
        }
    }

    /// Resolve a tag to the manifest for the host platform (descending
    /// through an image index if present).
    pub fn resolve(&self, repo: &str, tag: &str) -> Result<Resolved> {
        let reference = self.reference(repo, tag);
        let (r, o, a) = (
            cstring(&reference)?,
            cstring(crate::platform::os())?,
            cstring(crate::platform::arch())?,
        );
        let v = take_result(unsafe { PkgociResolve(r.as_ptr(), o.as_ptr(), a.as_ptr()) }).map_err(
            |e| {
                if e.to_string().contains("not found") {
                    anyhow!("package not found: {reference}")
                } else {
                    e
                }
            },
        )?;
        let manifest: Manifest = serde_json::from_value(v["manifest"].clone())?;
        let index: Option<Index> = v
            .get("index")
            .map(|i| serde_json::from_value(i.clone()))
            .transpose()?;
        let manifest_digest = v["digest"]
            .as_str()
            .ok_or_else(|| anyhow!("no digest in resolve result"))?
            .to_string();
        Ok(Resolved {
            manifest,
            manifest_digest,
            index,
        })
    }

    /// Stream a digest-verified blob to `dest`.
    pub fn download_blob(
        &self,
        repo: &str,
        tag: &str,
        desc: &Descriptor,
        dest: &Path,
    ) -> Result<()> {
        let (r, d, m, p) = (
            cstring(&self.reference(repo, tag))?,
            cstring(&desc.digest)?,
            cstring(&desc.media_type)?,
            cstring(&dest.to_string_lossy())?,
        );
        take_result(unsafe {
            PkgociFetchBlob(
                r.as_ptr(),
                d.as_ptr(),
                m.as_ptr(),
                desc.size as i64,
                p.as_ptr(),
            )
        })?;
        Ok(())
    }

    /// Upload a file as a blob (no-op if the registry already has the digest).
    pub fn push_blob(&self, repo: &str, tag: &str, digest: &str, path: &Path) -> Result<()> {
        let size = std::fs::metadata(path)?.len();
        let (r, d, p) = (
            cstring(&self.reference(repo, tag))?,
            cstring(digest)?,
            cstring(&path.to_string_lossy())?,
        );
        take_result(unsafe { PkgociPushBlob(r.as_ptr(), d.as_ptr(), size as i64, p.as_ptr()) })?;
        Ok(())
    }

    /// Upload a manifest or index under `tag`.
    pub fn push_manifest(&self, repo: &str, tag: &str, media_type: &str, body: &str) -> Result<()> {
        let (r, m, b) = (
            cstring(&self.reference(repo, tag))?,
            cstring(media_type)?,
            cstring(body)?,
        );
        take_result(unsafe { PkgociPushManifest(r.as_ptr(), m.as_ptr(), b.as_ptr()) })?;
        Ok(())
    }
}
