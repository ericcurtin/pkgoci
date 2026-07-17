//! Pkgocifile: a Dockerfile-style package description.
//!
//! ```text
//! NAME mytool
//! VERSION 1.2.3
//! DESCRIPTION My tool
//! LICENSE MIT
//! REQUIRES libfoo@^1.2
//! PLATFORM darwin/arm64 ./out/mac-arm64
//! PLATFORM linux/amd64 ./out/linux-amd64
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

pub const FILE_NAME: &str = "Pkgocifile";

pub struct Spec {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub url: Option<String>,
    pub requires: Vec<String>,
    /// (os, arch, payload directory) — paths relative to the Pkgocifile.
    pub platforms: Vec<(String, String, PathBuf)>,
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
    };
    if spec.platforms.is_empty() {
        bail!("{}: at least one PLATFORM is required", path.display());
    }
    if spec.name.contains('/') || spec.name.contains('@') {
        bail!("{}: NAME must be a plain package name", path.display());
    }
    Ok(spec)
}
