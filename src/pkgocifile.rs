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
//! # into the build context (.tar.gz/.tgz, .tar.bz2/.tbz2, .tar.zst/.tzst, or
//! # bare .tar; the leading path component is stripped only if every entry
//! # shares one, so plain sources and prebuilt vendor trees both just work);
//! # SOURCE publishes the tree so users on other platforms can build too;
//! # RUN/TEST execute sandboxed (RUN:<os> limits a step to one OS); FROM
//! # picks the Linux build image; lines may continue with a trailing
//! # backslash:
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
//!
//! One source tree can also produce **several** packages, the way a single
//! RPM or Debian source builds multiple binary packages: repeat `NAME` to
//! start a new one. `VERSION`, `FROM`, `ENV`, `FETCH`, `SOURCE`, `RUN`, and
//! `TEST` are shared by the whole file and run only once; `DESCRIPTION`,
//! `LICENSE`, `URL`, `REQUIRES`, `PLATFORM`, and `OUTPUT` apply to whichever
//! `NAME` came before them, so each package can pack a different slice of
//! the same build (e.g. `OUTPUT ./out-client` vs `OUTPUT ./out-server`).
//! All packages in a file always share one version; a bare `REQUIRES` on
//! another package defined in the same file is automatically pinned to that
//! shared version, so the family stays in lockstep across rebuilds without
//! hardcoding versions (see `examples/kubernetes/Pkgocifile`).

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

pub const FILE_NAME: &str = "Pkgocifile";

/// Archive suffixes FETCH knows how to decompress.
pub const ARCHIVE_SUFFIXES: &[&str] = &[
    ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst", ".tar",
];

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

/// One package produced by the (shared) build: a `NAME` block and
/// everything scoped to it until the next `NAME`.
pub struct Package {
    pub name: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub url: Option<String>,
    pub requires: Vec<String>,
    /// (os, arch, payload directory) — paths relative to the Pkgocifile.
    pub platforms: Vec<(String, String, PathBuf)>,
    /// Directory `run` produces, packed for the host platform (and used at
    /// install time when building from source). Default `./out`; when a
    /// file has several packages each needs its own.
    pub output: String,
}

impl Package {
    fn new(name: String) -> Self {
        Package {
            name,
            description: None,
            license: None,
            url: None,
            requires: Vec::new(),
            platforms: Vec::new(),
            output: "./out".to_string(),
        }
    }
}

pub struct Spec {
    pub version: String,
    /// Build commands executed sandboxed in the work tree, shared by every
    /// package in the file.
    pub run: Vec<Step>,
    /// Post-build checks executed sandboxed in the work tree.
    pub tests: Vec<Step>,
    /// Environment variables for RUN/TEST steps.
    pub env: Vec<(String, String)>,
    /// Source tree (relative to the build context) published with the
    /// package(s) for build-from-source installs.
    pub source: Option<String>,
    /// Upstream (url, sha256) tarballs extracted into the context before
    /// anything else runs.
    pub fetches: Vec<(String, String)>,
    /// Container image for sandboxed Linux builds (like Dockerfile FROM).
    pub image: String,
    /// One or more packages described by this file, in `NAME` order.
    pub packages: Vec<Package>,
}

impl Spec {
    /// The file's primary package (the first `NAME`).
    pub fn primary(&self) -> &Package {
        &self.packages[0]
    }
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

    let mut version = None;
    let mut run: Vec<Step> = Vec::new();
    let mut tests: Vec<Step> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut source = None;
    let mut fetches = Vec::new();
    let mut image = crate::sandbox::DEFAULT_IMAGE.to_string();
    let mut packages: Vec<Package> = Vec::new();

    for (lineno, line) in logical_lines(&text) {
        let (directive, rest) = match line.split_once(char::is_whitespace) {
            Some((d, r)) => (d, r.trim().to_string()),
            None => (line.as_str(), String::new()),
        };
        if rest.is_empty() && directive != "SOURCE" {
            return Err(err(lineno, format!("directive {directive:?} has no value")));
        }
        // Everything except the file-wide directives below is scoped to
        // the most recent NAME; PLATFORM/OUTPUT/etc. before any NAME is
        // an error rather than silently doing nothing.
        fn current<'a>(
            lineno: usize,
            path: &Path,
            packages: &'a mut [Package],
        ) -> Result<&'a mut Package> {
            packages
                .last_mut()
                .ok_or_else(|| anyhow!("{}:{}: NAME must come first", path.display(), lineno + 1))
        }
        match directive {
            "NAME" => packages.push(Package::new(rest)),
            "VERSION" => {
                if version.is_some() {
                    return Err(err(lineno, "VERSION already set".into()));
                }
                semver::Version::parse(&rest)
                    .map_err(|e| err(lineno, format!("VERSION {rest:?}: {e}")))?;
                version = Some(rest);
            }
            "DESCRIPTION" => current(lineno, path, &mut packages)?.description = Some(rest),
            "LICENSE" => current(lineno, path, &mut packages)?.license = Some(rest),
            "URL" => current(lineno, path, &mut packages)?.url = Some(rest),
            "REQUIRES" => {
                let pkg = current(lineno, path, &mut packages)?;
                for req in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let (_, constraint) = crate::resolve::parse_requirement(req);
                    crate::resolve::parse_range(constraint.as_deref())
                        .map_err(|e| err(lineno, format!("REQUIRES {req}: {e}")))?;
                    pkg.requires.push(req.to_string());
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
                current(lineno, path, &mut packages)?.platforms.push((
                    os.to_string(),
                    arch.to_string(),
                    dir,
                ));
            }
            "OUTPUT" => current(lineno, path, &mut packages)?.output = rest,
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
                let lower = url.to_ascii_lowercase();
                if !ARCHIVE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
                    return Err(err(
                        lineno,
                        format!(
                            "FETCH url {url:?} must end in one of {}",
                            ARCHIVE_SUFFIXES.join(", ")
                        ),
                    ));
                }
                fetches.push((url.to_string(), sha.to_string()));
            }
            d if d == "RUN" || d.starts_with("RUN:") => run.push(step(lineno, d, rest)?),
            d if d == "TEST" || d.starts_with("TEST:") => tests.push(step(lineno, d, rest)?),
            other => return Err(err(lineno, format!("unknown directive {other:?}"))),
        }
    }

    if packages.is_empty() {
        bail!("{}: missing NAME", path.display());
    }
    let version = version.ok_or_else(|| anyhow!("{}: missing VERSION", path.display()))?;

    // A bare REQUIRES on another package defined in this same file always
    // means "the copy we just built together", so it is pinned to the
    // shared version rather than left unconstrained.
    let sibling_names: std::collections::HashSet<String> =
        packages.iter().map(|p| p.name.clone()).collect();
    for pkg in &mut packages {
        for req in &mut pkg.requires {
            let (name, constraint) = crate::resolve::parse_requirement(req);
            if constraint.is_none() && name != pkg.name && sibling_names.contains(name.as_str()) {
                *req = format!("{name}@{version}");
            }
        }
    }

    for pkg in &packages {
        if pkg.platforms.is_empty() && run.is_empty() {
            bail!(
                "{}: {} needs at least one PLATFORM or a shared RUN",
                path.display(),
                pkg.name
            );
        }
    }
    if packages.len() > 1 {
        let mut seen = std::collections::HashMap::new();
        for pkg in &packages {
            if pkg.platforms.is_empty() {
                if let Some(prev) = seen.insert(pkg.output.clone(), pkg.name.clone()) {
                    bail!(
                        "{}: {} and {} both use OUTPUT {}; each package in a multi-package file needs its own",
                        path.display(),
                        prev,
                        pkg.name,
                        pkg.output
                    );
                }
            }
        }
    }
    if source.is_some() && run.is_empty() {
        bail!("{}: SOURCE requires RUN build steps", path.display());
    }
    if !tests.is_empty() && run.is_empty() {
        bail!("{}: TEST requires RUN build steps", path.display());
    }

    Ok(Spec {
        version,
        run,
        tests,
        env,
        source,
        fetches,
        image,
        packages,
    })
}
