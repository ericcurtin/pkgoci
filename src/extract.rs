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
