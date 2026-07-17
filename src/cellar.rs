use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Install receipt written into every keg.
#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub name: String,
    pub repo: String,
    pub version: String,
    pub tag: String,
    pub manifest_digest: String,
    pub installed_at: u64,
    /// Paths linked into the prefix, relative to the prefix (e.g. `bin/jq`).
    pub linked: Vec<String>,
    /// Package names this package requires at runtime.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

pub const RECEIPT_FILE: &str = ".pkgoci-receipt.json";

pub fn keg_path(cfg: &Config, name: &str, version: &str) -> PathBuf {
    cfg.cellar().join(name).join(version)
}

pub fn read_receipt(cfg: &Config, name: &str, version: &str) -> Option<Receipt> {
    let data = fs::read(keg_path(cfg, name, version).join(RECEIPT_FILE)).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn write_receipt(cfg: &Config, receipt: &Receipt) -> Result<()> {
    let path = keg_path(cfg, &receipt.name, &receipt.version).join(RECEIPT_FILE);
    fs::write(&path, serde_json::to_vec_pretty(receipt)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn installed_versions(cfg: &Config, name: &str) -> Vec<String> {
    let dir = cfg.cellar().join(name);
    let mut versions: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    versions
}

/// All installed packages as (name, versions).
pub fn list_installed(cfg: &Config) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = fs::read_dir(cfg.cellar())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let versions = installed_versions(cfg, &name);
            (name, versions)
        })
        .filter(|(_, v)| !v.is_empty())
        .collect();
    out.sort();
    out
}

/// Link executables from `<keg>/bin` into `<prefix>/bin`.
/// Returns prefix-relative paths of created links.
pub fn link_keg(cfg: &Config, name: &str, version: &str) -> Result<Vec<String>> {
    let keg_bin = keg_path(cfg, name, version).join("bin");
    let prefix_bin = cfg.bin();
    fs::create_dir_all(&prefix_bin)?;
    let mut linked = Vec::new();
    for entry in fs::read_dir(&keg_bin).into_iter().flatten().flatten() {
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let dst = prefix_bin.join(&file_name);
        make_link(&src, &dst)?;
        linked.push(format!("bin/{}", file_name.to_string_lossy()));
    }
    Ok(linked)
}

#[cfg(unix)]
fn make_link(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if dst.symlink_metadata().is_ok() {
        fs::remove_file(dst)?;
    }
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("linking {} -> {}", dst.display(), src.display()))?;
    Ok(())
}

#[cfg(windows)]
fn make_link(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if dst.exists() {
        fs::remove_file(dst)?;
    }
    // Hard links avoid the symlink privilege requirement on Windows.
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

/// Remove everything a receipt linked into the prefix.
pub fn unlink(cfg: &Config, receipt: &Receipt) {
    for rel in &receipt.linked {
        let _ = fs::remove_file(cfg.prefix.join(rel));
    }
}

/// Remove a keg directory (and the package dir if now empty).
pub fn remove_keg(cfg: &Config, name: &str, version: &str) -> Result<()> {
    let keg = keg_path(cfg, name, version);
    fs::remove_dir_all(&keg).with_context(|| format!("removing {}", keg.display()))?;
    let pkg_dir = cfg.cellar().join(name);
    if fs::read_dir(&pkg_dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir(&pkg_dir);
    }
    Ok(())
}

pub fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{val:.1}{}", UNITS[unit])
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
