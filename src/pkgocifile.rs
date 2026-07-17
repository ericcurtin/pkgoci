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
//! # Or build from source. FETCH downloads a digest-pinned upstream tarball
//! # into the build context; SOURCE publishes the tree so users on other
//! # platforms can build too; RUN/TEST execute sandboxed (RUN:<os> limits a
//! # step to one OS); FROM picks the Linux build image; lines may continue
//! # with a trailing backslash:
//! FROM docker.io/library/buildpack-deps:bookworm
//! FETCH https://example.com/mytool-${PKGOCI_VERSION}.tar.gz <sha256>
//! SOURCE
//! ENV CFLAGS=-O2
//! RUN ./configure --prefix=$PWD/out \
//!     --disable-nls
//! RUN make -j4 install
//! TEST ./out/bin/mytool --version
//! OUTPUT ./out
//! ```

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

pub const FILE_NAME: &str = "Pkgocifile";

/// A build or test step, optionally limited to one OS (`RUN:linux ...`).
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
    /// Build commands executed sandboxed in the work tree.
    pub run: Vec<Step>,
    /// Post-build checks executed sandboxed in the work tree.
    pub tests: Vec<Step>,
    /// Environment variables for RUN/TEST steps.
    pub env: Vec<(String, String)>,
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

/// Join backslash-continued lines, keeping the starting line number.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if pending.is_none() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        let (line, continued) = match line.strip_suffix('\\') {
            Some(rest) => (rest.trim_end(), true),
            None => (line, false),
        };
        let (start, acc) = match pending.take() {
            Some((start, mut acc)) => {
                acc.push(' ');
                acc.push_str(line);
                (start, acc)
            }
            None => (i, line.to_string()),
        };
        if continued {
            pending = Some((start, acc));
        } else {
            out.push((start, acc));
        }
    }
    if let Some(p) = pending {
        out.push(p);
    }
    out
}

pub fn parse(path: &Path) -> Result<Spec> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let err = |lineno: usize, msg: String| anyhow!("{}:{}: {msg}", path.display(), lineno + 1);
    let step = |lineno: usize, directive: &str, cmd: String| -> Result<Step> {
        match directive.split_once(':') {
            None => Ok(Step { os: None, cmd }),
            Some((_, os)) if ["darwin", "linux", "windows"].contains(&os) => Ok(Step {
                os: Some(os.to_string()),
                cmd,
            }),
            Some((_, os)) => Err(err(lineno, format!("unknown OS {os:?}"))),
        }
    };

    let mut name = None;
    let mut version = None;
    let mut description = None;
    let mut license = None;
    let mut url = None;
    let mut requires = Vec::new();
    let mut platforms: Vec<(String, String, PathBuf)> = Vec::new();
    let mut run: Vec<Step> = Vec::new();
    let mut tests: Vec<Step> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut output = "./out".to_string();
    let mut source = None;
    let mut fetches = Vec::new();
    let mut image = crate::sandbox::DEFAULT_IMAGE.to_string();

    for (lineno, line) in logical_lines(&text) {
        let (directive, rest) = match line.split_once(char::is_whitespace) {
            Some((d, r)) => (d, r.trim().to_string()),
            None => (line.as_str(), String::new()),
        };
        if rest.is_empty() && directive != "SOURCE" {
            return Err(err(lineno, format!("directive {directive:?} has no value")));
        }
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
            "OUTPUT" => output = rest,
            "FROM" => image = rest,
            "ENV" => {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| err(lineno, "ENV needs KEY=VALUE".into()))?;
                env.push((k.trim().to_string(), v.trim().to_string()));
            }
            "SOURCE" => {
                let rel = if rest.is_empty() {
                    ".".to_string()
                } else {
                    rest
                };
                if !base.join(&rel).is_dir() {
                    return Err(err(lineno, format!("no such directory: {rel}")));
                }
                source = Some(rel);
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
            d if d == "RUN" || d.starts_with("RUN:") => run.push(step(lineno, d, rest)?),
            d if d == "TEST" || d.starts_with("TEST:") => tests.push(step(lineno, d, rest)?),
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
        tests,
        env,
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
    if !spec.tests.is_empty() && spec.run.is_empty() {
        bail!("{}: TEST requires RUN build steps", path.display());
    }
    Ok(spec)
}
