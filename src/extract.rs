use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::oci;

/// Extract an OCI layer archive into `dest` based on its media type.
pub fn extract_layer(archive: &Path, media_type: &str, dest: &Path) -> Result<()> {
    let file = BufReader::new(
        File::open(archive).with_context(|| format!("opening {}", archive.display()))?,
    );
    let reader: Box<dyn Read> = match media_type {
        oci::MT_LAYER_TAR_GZIP | "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            Box::new(flate2::read::GzDecoder::new(file))
        }
        oci::MT_LAYER_TAR_ZSTD => Box::new(ruzstd::decoding::StreamingDecoder::new(file)?),
        "application/vnd.oci.image.layer.v1.tar" => Box::new(file),
        other => bail!("unsupported layer media type: {other}"),
    };
    std::fs::create_dir_all(dest)?;
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.unpack(dest)
        .with_context(|| format!("extracting into {}", dest.display()))?;
    Ok(())
}

fn decompress<'a>(bytes: &'a [u8], url: &str) -> Result<Box<dyn Read + 'a>> {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Ok(Box::new(flate2::read::GzDecoder::new(bytes)))
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        Ok(Box::new(bzip2_rs::DecoderReader::new(bytes)))
    } else if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        Ok(Box::new(ruzstd::decoding::StreamingDecoder::new(bytes)?))
    } else if lower.ends_with(".tar") {
        Ok(Box::new(bytes))
    } else {
        // pkgocifile::parse already rejects unsupported extensions, so this
        // only fires for a hand-built Spec.
        bail!(
            "unsupported FETCH archive extension in {url:?} (expected one of {})",
            crate::pkgocifile::ARCHIVE_SUFFIXES.join(", ")
        )
    }
}

/// Extract a FETCH archive (`.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`,
/// `.tar.zst`/`.tzst`, or bare `.tar`) into `dest`. The leading path
/// component is stripped (like `tar --strip-components=1`) only when every
/// entry shares the same one, so both upstream release tarballs (one
/// wrapping directory) and vendor/dependency trees (no wrapping directory,
/// e.g. Fedora's `go-vendor-tools` archives) extract correctly without
/// needing a flag.
pub fn extract_fetch_archive(bytes: &[u8], url: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    // Pseudo-entries (pax global/extended headers) describe metadata for
    // the entry that follows, not a real path; ignore them when guessing
    // the common wrapping directory and when unpacking.
    let is_real =
        |t: tar::EntryType| !matches!(t, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader);

    let mut common: Option<Option<std::ffi::OsString>> = None;
    for entry in tar::Archive::new(decompress(bytes, url)?).entries()? {
        let entry = entry?;
        if !is_real(entry.header().entry_type()) {
            continue;
        }
        let first = entry
            .path()?
            .components()
            .next()
            .map(|c| c.as_os_str().to_owned());
        match (&common, first) {
            (None, first) => common = Some(first),
            (Some(Some(c)), Some(f)) if *c == f => {}
            (Some(_), _) => common = Some(None),
        }
    }
    let strip = matches!(common, Some(Some(_)));

    let mut ar = tar::Archive::new(decompress(bytes, url)?);
    ar.set_preserve_permissions(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if !is_real(entry.header().entry_type()) {
            continue;
        }
        let path = entry.path()?.into_owned();
        let rel: std::path::PathBuf = if strip {
            path.components().skip(1).collect()
        } else {
            path
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        entry.unpack(dest.join(rel))?;
    }
    Ok(())
}

/// Pack a directory into a `.tar.gz` layer (used by `pkgoci push`).
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", dir)
        .with_context(|| format!("packing {}", dir.display()))?;
    Ok(builder.into_inner()?.finish()?)
}
