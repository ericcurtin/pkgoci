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

/// Extract a `.tar.gz` with the leading path component stripped (like
/// `tar --strip-components=1`), used for upstream source tarballs.
pub fn extract_tar_gz_strip1(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    ar.set_preserve_permissions(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let stripped: std::path::PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        entry.unpack(dest.join(stripped))?;
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
