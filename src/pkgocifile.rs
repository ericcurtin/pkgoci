//! Pkgocifile: a Dockerfile-style package description.
//!
//! ```text
//! NAME mytool
//! VERSION 1.2.3
//! DESCRIPTION My tool
//! LICENSE MIT
//! REQUIRES libfoo@^1.2
//! # Prebuilt trees:
//! PLATFORM darwin/arm64 ./out/mac-arm64
//! PLATFORM linux/amd64 ./out/linux-amd64
//! # Or build from source (RUN executes on the host at build time; SOURCE is
//! # published with the package so users on other platforms can build too).
//! # FETCH downloads a digest-pinned upstream tarball into the context, and
//! # RUN:<os> limits a step to one OS:
//! FETCH https://example.com/mytool-${PKGOCI_VERSION}.tar.gz <sha256>
//! SOURCE .
//! RUN:darwin make macosx
//! RUN:linux make linux
//! RUN make install INSTALL_TOP=$PWD/out
//! OUTPUT ./out
//! ```

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

pub const FILE_NAME: &str = "Pkgocifile";

/// A build step, optionally limited to one OS (`RUN:linux ...`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    pub cmd: String,
}

impl Step {
    /// Does this step apply to the given OS?
    pub fn applies_to(&self, os: &str) -> bool {
        self.os.as_deref().is_none_or(|o| o == os)
    }
}

pub struct Spec {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub url: Option<String>,
    pub requires: Vec<String>,
    /// (os, arch, payload directory) — paths relative to the Pkgocifile.
    pub platforms: Vec<(String, String, PathBuf)>,
    /// Build commands executed in the Pkgocifile's directory.
    pub run: Vec<Step>,
    /// Directory `run` produces, packed for the host platform
    /// (and used at install time when building from source).
    pub output: String,
    /// Source tree (relative to the build context) published with the
    /// package for build-from-source installs.
    pub source: Option<String>,
    /// Upstream (url, sha256) tarballs extracted into the context before
    /// anything else runs.
    pub fetches: Vec<(String, String)>,
    /// Container image for sandboxed Linux builds (like Dockerfile FROM).
    pub image: String,
}

pub fn parse(path: &Path) -> Result<Spec> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let err = |lineno: usize, msg: String| anyhow!("{}:{}: {msg}", path.display(), lineno + 1);

    let mut name = None;
    let mut version = None;
    let mut description = None;
    let mut license = None;
    let mut url = None;
    let mut requires = Vec::new();
    let mut platforms: Vec<(String, String, PathBuf)> = Vec::new();
    let mut run: Vec<Step> = Vec::new();
    let mut output = "./out".to_string();
    let mut source = None;
    let mut fetches = Vec::new();
    let mut image = crate::sandbox::DEFAULT_IMAGE.to_string();

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (directive, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| err(lineno, format!("directive {line:?} has no value")))?;
        let rest = rest.trim().to_string();
        match directive {
            "NAME" => name = Some(rest),
            "VERSION" => {
                semver::Version::parse(&rest)
                    .map_err(|e| err(lineno, format!("VERSION {rest:?}: {e}")))?;
                version = Some(rest);
            }
            "DESCRIPTION" => description = Some(rest),
            "LICENSE" => license = Some(rest),
            "URL" => url = Some(rest),
            "REQUIRES" => {
                for req in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let (_, constraint) = crate::resolve::parse_requirement(req);
                    crate::resolve::parse_range(constraint.as_deref())
                        .map_err(|e| err(lineno, format!("REQUIRES {req}: {e}")))?;
                    requires.push(req.to_string());
                }
            }
            "PLATFORM" => {
                let (platform, dir) = rest
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| err(lineno, "PLATFORM needs `os/arch <dir>`".into()))?;
                let (os, arch) = platform.split_once('/').ok_or_else(|| {
                    err(
                        lineno,
                        format!("platform must be os/arch, got {platform:?}"),
                    )
                })?;
                let dir = base.join(dir.trim());
                if !dir.is_dir() {
                    return Err(err(lineno, format!("no such directory: {}", dir.display())));
                }
                platforms.push((os.to_string(), arch.to_string(), dir));
            }
            "RUN" => run.push(Step {
                os: None,
                cmd: rest,
            }),
            "OUTPUT" => output = rest,
            "IMAGE" => image = rest,
            "SOURCE" => {
                if !base.join(&rest).is_dir() {
                    return Err(err(lineno, format!("no such directory: {rest}")));
                }
                source = Some(rest);
            }
            "FETCH" => {
                let (url, sha) = rest
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| err(lineno, "FETCH needs `<url> <sha256>`".into()))?;
                let sha = sha.trim();
                if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(err(
                        lineno,
                        format!("FETCH sha256 {sha:?} is not a hex digest"),
                    ));
                }
                fetches.push((url.to_string(), sha.to_string()));
            }
            run_os if run_os.starts_with("RUN:") => {
                let os = &run_os[4..];
                if !["darwin", "linux", "windows"].contains(&os) {
                    return Err(err(lineno, format!("unknown RUN OS {os:?}")));
                }
                run.push(Step {
                    os: Some(os.to_string()),
                    cmd: rest,
                });
            }
            other => return Err(err(lineno, format!("unknown directive {other:?}"))),
        }
    }

    let spec = Spec {
        name: name.ok_or_else(|| anyhow!("{}: missing NAME", path.display()))?,
        version: version.ok_or_else(|| anyhow!("{}: missing VERSION", path.display()))?,
        description,
        license,
        url,
        requires,
        platforms,
        run,
        output,
        source,
        fetches,
        image,
    };
    if spec.platforms.is_empty() && spec.run.is_empty() {
        bail!(
            "{}: at least one PLATFORM or RUN is required",
            path.display()
        );
    }
    if spec.source.is_some() && spec.run.is_empty() {
        bail!("{}: SOURCE requires RUN build steps", path.display());
    }
    if spec.name.contains('/') || spec.name.contains('@') {
        bail!("{}: NAME must be a plain package name", path.display());
    }
    Ok(spec)
}
