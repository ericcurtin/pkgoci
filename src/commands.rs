use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::cellar::{self, Receipt};
use crate::config::Config;
use crate::extract;
use crate::oci;
use crate::registry::Client;

/// Split `name@version` into (name, tag).
fn parse_spec(spec: &str) -> (String, String) {
    match spec.rsplit_once('@') {
        Some((name, version)) if !version.is_empty() && !version.contains('/') => {
            (name.to_string(), version.to_string())
        }
        _ => (spec.to_string(), "latest".to_string()),
    }
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{msg:20} {bar:30} {bytes}/{total_bytes} {bytes_per_sec}").unwrap()
}

// ---------------------------------------------------------------- install

pub async fn install(cfg: &Config, packages: Vec<String>, force: bool) -> Result<()> {
    if packages.is_empty() {
        bail!("no packages given");
    }
    let start = Instant::now();
    let client = Arc::new(Client::new(&cfg.registry)?);
    let cfg = Arc::new(Config {
        prefix: cfg.prefix.clone(),
        registry: cfg.registry.clone(),
        namespace: cfg.namespace.clone(),
    });
    let mp = MultiProgress::new();

    let tasks: Vec<_> = packages
        .iter()
        .map(|spec| {
            let (client, cfg, mp, spec) = (client.clone(), cfg.clone(), mp.clone(), spec.clone());
            tokio::spawn(async move { install_one(&cfg, &client, &spec, force, &mp).await })
        })
        .collect();

    let mut failures = 0;
    for (task, spec) in tasks.into_iter().zip(&packages) {
        if let Err(e) = task.await? {
            eprintln!("error: {spec}: {e:#}");
            failures += 1;
        }
    }
    if failures > 0 {
        bail!("{failures} package(s) failed to install");
    }
    println!("Done in {:.2}s", start.elapsed().as_secs_f64());
    Ok(())
}

async fn install_one(
    cfg: &Config,
    client: &Client,
    spec: &str,
    force: bool,
    mp: &MultiProgress,
) -> Result<()> {
    let (name, tag) = parse_spec(spec);
    let repo = cfg.repo_for(&name);
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();

    let resolved = client.resolve(&repo, &tag).await?;
    let version = resolved.version(&tag);

    if !force && cellar::read_receipt(cfg, &short, &version).is_some() {
        println!("{short} {version} is already installed");
        return Ok(());
    }

    let layer = resolved
        .manifest
        .layers
        .first()
        .ok_or_else(|| anyhow!("manifest for {repo}:{tag} has no layers"))?
        .clone();

    // Download (or reuse cached, digest-verified archive).
    std::fs::create_dir_all(cfg.cache())?;
    let ext = if layer.media_type.contains("zstd") {
        "tar.zst"
    } else {
        "tar.gz"
    };
    let cache_file = cfg.cache().join(format!(
        "{}-{}-{}.{ext}",
        short,
        version,
        &layer.digest.trim_start_matches("sha256:")[..12]
    ));
    if !cache_file.exists() {
        let pb = mp.add(
            ProgressBar::new(layer.size)
                .with_style(bar_style())
                .with_message(short.clone()),
        );
        client
            .download_blob(&repo, &layer, &cache_file, &pb)
            .await?;
        pb.finish_and_clear();
    }

    // Extract into the keg.
    let keg = cellar::keg_path(cfg, &short, &version);
    if keg.exists() {
        // Unlink a previous install of the same version before replacing it.
        if let Some(old) = cellar::read_receipt(cfg, &short, &version) {
            cellar::unlink(cfg, &old);
        }
        std::fs::remove_dir_all(&keg)?;
    }
    let (media_type, keg_clone, cache_clone) =
        (layer.media_type.clone(), keg.clone(), cache_file.clone());
    tokio::task::spawn_blocking(move || {
        extract::extract_layer(&cache_clone, &media_type, &keg_clone)
    })
    .await??;

    // Remove any other installed version's links, then link the new keg.
    for old_version in cellar::installed_versions(cfg, &short) {
        if old_version != version {
            if let Some(old) = cellar::read_receipt(cfg, &short, &old_version) {
                cellar::unlink(cfg, &old);
            }
        }
    }
    let linked = cellar::link_keg(cfg, &short, &version)?;

    cellar::write_receipt(
        cfg,
        &Receipt {
            name: short.clone(),
            repo,
            version: version.clone(),
            tag,
            manifest_digest: resolved.manifest_digest.clone(),
            installed_at: cellar::now_unix(),
            linked: linked.clone(),
        },
    )?;

    println!(
        "Installed {short} {version} ({}, {} linked)",
        cellar::human_bytes(cellar::dir_size(&keg)),
        linked.len()
    );
    Ok(())
}

// -------------------------------------------------------------- uninstall

pub fn uninstall(cfg: &Config, packages: Vec<String>) -> Result<()> {
    if packages.is_empty() {
        bail!("no packages given");
    }
    for spec in packages {
        let (name, _) = parse_spec(&spec);
        let short = name.rsplit('/').next().unwrap_or(&name).to_string();
        let versions = cellar::installed_versions(cfg, &short);
        if versions.is_empty() {
            eprintln!("error: {short} is not installed");
            continue;
        }
        for version in versions {
            if let Some(receipt) = cellar::read_receipt(cfg, &short, &version) {
                cellar::unlink(cfg, &receipt);
            }
            cellar::remove_keg(cfg, &short, &version)?;
            println!("Uninstalled {short} {version}");
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- list

pub fn list(cfg: &Config) -> Result<()> {
    for (name, versions) in cellar::list_installed(cfg) {
        println!("{name} {}", versions.join(" "));
    }
    Ok(())
}

// ------------------------------------------------------------------- info

pub async fn info(cfg: &Config, package: String) -> Result<()> {
    let (name, tag) = parse_spec(&package);
    let repo = cfg.repo_for(&name);
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let client = Client::new(&cfg.registry)?;
    let resolved = client.resolve(&repo, &tag).await?;
    let version = resolved.version(&tag);

    println!("{short}: {version}");
    if let Some(desc) = resolved.annotation(oci::ANNOTATION_DESCRIPTION) {
        println!("{desc}");
    }
    if let Some(url) = resolved.annotation(oci::ANNOTATION_URL) {
        println!("{url}");
    }
    if let Some(license) = resolved.annotation(oci::ANNOTATION_LICENSES) {
        println!("License: {license}");
    }
    println!("Source: {}/{repo}:{tag}", cfg.registry);
    if let Some(index) = &resolved.index {
        println!("Platforms: {}", index.platforms().join(", "));
    }
    if let Some(layer) = resolved.manifest.layers.first() {
        println!(
            "Download: {} ({})",
            cellar::human_bytes(layer.size),
            layer.media_type
        );
    }
    let installed = cellar::installed_versions(cfg, &short);
    if installed.is_empty() {
        println!("Not installed");
    } else {
        for v in installed {
            println!("Installed: {}", cellar::keg_path(cfg, &short, &v).display());
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- search

pub async fn search(cfg: &Config, term: String) -> Result<()> {
    if !cfg.is_docker_hub() {
        bail!(
            "search is only supported on Docker Hub (registry: {})",
            cfg.registry
        );
    }
    let http = reqwest::Client::builder()
        .user_agent(concat!("pkgoci/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut url = format!(
        "https://hub.docker.com/v2/repositories/{}/?page_size=100",
        cfg.namespace
    );
    let mut found = 0;
    loop {
        let v: serde_json::Value = http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("listing repositories under {}", cfg.namespace))?
            .json()
            .await?;
        for repo in v
            .get("results")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
        {
            let name = repo.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let desc = repo
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            if name.contains(&term) || desc.to_lowercase().contains(&term.to_lowercase()) {
                if desc.is_empty() {
                    println!("{name}");
                } else {
                    println!("{name}: {desc}");
                }
                found += 1;
            }
        }
        match v.get("next").and_then(|n| n.as_str()) {
            Some(next) => url = next.to_string(),
            None => break,
        }
    }
    if found == 0 {
        println!("No packages matching \"{term}\" in {}", cfg.namespace);
    }
    Ok(())
}

// ---------------------------------------------------------------- upgrade

pub async fn upgrade(cfg: &Config, packages: Vec<String>) -> Result<()> {
    let targets: Vec<String> = if packages.is_empty() {
        cellar::list_installed(cfg)
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    } else {
        packages
    };
    if targets.is_empty() {
        println!("Nothing installed.");
        return Ok(());
    }

    let client = Client::new(&cfg.registry)?;
    let mut outdated = Vec::new();
    for name in &targets {
        let (name, _) = parse_spec(name);
        let short = name.rsplit('/').next().unwrap_or(&name).to_string();
        let versions = cellar::installed_versions(cfg, &short);
        let Some(current) = versions.last().cloned() else {
            eprintln!("error: {short} is not installed");
            continue;
        };
        let receipt = cellar::read_receipt(cfg, &short, &current);
        let repo = receipt
            .as_ref()
            .map(|r| r.repo.clone())
            .unwrap_or_else(|| cfg.repo_for(&short));
        let resolved = client.resolve(&repo, "latest").await?;
        let latest = resolved.version("latest");
        if latest != current {
            println!("{short} {current} -> {latest}");
            outdated.push((short, current));
        }
    }
    if outdated.is_empty() {
        println!("Everything is up to date.");
        return Ok(());
    }

    install(
        cfg,
        outdated.iter().map(|(n, _)| n.clone()).collect(),
        false,
    )
    .await?;
    for (name, old_version) in outdated {
        if cellar::installed_versions(cfg, &name).len() > 1 {
            cellar::remove_keg(cfg, &name, &old_version)?;
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- update

pub fn update() -> Result<()> {
    // pkgoci has no local package index: metadata is resolved live from the
    // registry, so there is nothing to sync.
    println!("Already up to date (pkgoci resolves packages live from the registry).");
    Ok(())
}

// ---------------------------------------------------------------- cleanup

pub fn cleanup(cfg: &Config) -> Result<()> {
    let mut freed = 0u64;

    // Drop the download cache.
    let cache = cfg.cache();
    if cache.exists() {
        freed += cellar::dir_size(&cache);
        std::fs::remove_dir_all(&cache)?;
    }

    // Drop all but the newest version of each package.
    for (name, versions) in cellar::list_installed(cfg) {
        let (old, newest) = versions.split_at(versions.len() - 1);
        for version in old {
            let keg = cellar::keg_path(cfg, &name, version);
            freed += cellar::dir_size(&keg);
            if let Some(receipt) = cellar::read_receipt(cfg, &name, version) {
                cellar::unlink(cfg, &receipt);
            }
            cellar::remove_keg(cfg, &name, version)?;
            println!("Removed {name} {version}");
        }
        // Ensure the surviving version is the one linked.
        if !old.is_empty() {
            if let Some(mut receipt) = cellar::read_receipt(cfg, &name, &newest[0]) {
                receipt.linked = cellar::link_keg(cfg, &name, &newest[0])?;
                cellar::write_receipt(cfg, &receipt)?;
            }
        }
    }
    println!("Freed {}", cellar::human_bytes(freed));
    Ok(())
}

// ------------------------------------------------------------------- push

/// Publish a directory as a (multi-platform) package.
/// `platform_dirs` entries look like `darwin/arm64=./out/mac-arm64`.
pub async fn push(
    cfg: &Config,
    name: String,
    version: String,
    platform_dirs: Vec<String>,
    description: Option<String>,
    license: Option<String>,
) -> Result<()> {
    if platform_dirs.is_empty() {
        bail!("at least one --dir os/arch=path is required");
    }
    let repo = cfg.repo_for(&name);
    let client = Client::new(&cfg.registry)?;

    let mut annotations = oci::Annotations::new();
    annotations.insert(oci::ANNOTATION_VERSION.into(), version.clone());
    if let Some(d) = &description {
        annotations.insert(oci::ANNOTATION_DESCRIPTION.into(), d.clone());
    }
    if let Some(l) = &license {
        annotations.insert(oci::ANNOTATION_LICENSES.into(), l.clone());
    }

    // Shared empty config blob.
    let config_bytes = b"{}".to_vec();
    let config_digest = format!("sha256:{}", oci::sha256_hex(&config_bytes));
    client
        .push_blob(&repo, &config_digest, config_bytes.clone())
        .await?;

    let mut manifests = Vec::new();
    for entry in &platform_dirs {
        let (platform, dir) = entry
            .split_once('=')
            .ok_or_else(|| anyhow!("--dir must be os/arch=path, got: {entry}"))?;
        let (os, arch) = platform
            .split_once('/')
            .ok_or_else(|| anyhow!("platform must be os/arch, got: {platform}"))?;

        let layer_bytes = extract::pack_dir(std::path::Path::new(dir))?;
        let layer_digest = format!("sha256:{}", oci::sha256_hex(&layer_bytes));
        let layer_size = layer_bytes.len() as u64;
        println!(
            "Uploading {platform} layer ({})...",
            cellar::human_bytes(layer_size)
        );
        client.push_blob(&repo, &layer_digest, layer_bytes).await?;

        let manifest = oci::Manifest {
            schema_version: 2,
            media_type: Some(oci::MT_OCI_MANIFEST.into()),
            config: oci::Descriptor {
                media_type: oci::MT_OCI_CONFIG.into(),
                digest: config_digest.clone(),
                size: config_bytes.len() as u64,
                platform: None,
                annotations: None,
            },
            layers: vec![oci::Descriptor {
                media_type: oci::MT_LAYER_TAR_GZIP.into(),
                digest: layer_digest,
                size: layer_size,
                platform: None,
                annotations: None,
            }],
            annotations: Some(annotations.clone()),
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let manifest_digest = format!("sha256:{}", oci::sha256_hex(&manifest_bytes));
        client
            .push_manifest(
                &repo,
                &manifest_digest,
                oci::MT_OCI_MANIFEST,
                manifest_bytes.clone(),
            )
            .await?;

        manifests.push(oci::Descriptor {
            media_type: oci::MT_OCI_MANIFEST.into(),
            digest: manifest_digest,
            size: manifest_bytes.len() as u64,
            platform: Some(oci::Platform {
                os: os.into(),
                architecture: arch.into(),
            }),
            annotations: None,
        });
    }

    let index = oci::Index {
        schema_version: 2,
        media_type: Some(oci::MT_OCI_INDEX.into()),
        manifests,
        annotations: Some(annotations),
    };
    let index_bytes = serde_json::to_vec(&index)?;
    for tag in [version.as_str(), "latest"] {
        client
            .push_manifest(&repo, tag, oci::MT_OCI_INDEX, index_bytes.clone())
            .await?;
    }
    println!(
        "Pushed {}/{repo}:{version} ({} platform(s))",
        cfg.registry,
        index.manifests.len()
    );
    Ok(())
}
