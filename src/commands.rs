use std::collections::HashSet;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

use crate::cellar::{self, Receipt};
use crate::config::Config;
use crate::extract;
use crate::oci;
use crate::registry::{Client, Resolved};
use crate::resolve::{self, parse_range, parse_requirement, requirements, Provider};
use crate::sign;

/// Split `name@version` into (name, tag).
fn parse_spec(spec: &str) -> (String, String) {
    match spec.rsplit_once('@') {
        Some((name, version)) if !version.is_empty() && !version.contains('/') => {
            (name.to_string(), version.to_string())
        }
        _ => (spec.to_string(), "latest".to_string()),
    }
}

/// Short package name (basename) of a spec.
fn short_name(spec: &str) -> String {
    let (name, _) = parse_spec(spec);
    name.rsplit('/').next().unwrap_or(&name).to_string()
}

// ---------------------------------------------------------------- install

pub fn install(cfg: &Config, packages: Vec<String>, force: bool, from_source: bool) -> Result<()> {
    if packages.is_empty() {
        bail!("no packages given");
    }
    let start = Instant::now();
    let client = Client::new(&cfg.registry);
    let requested: HashSet<String> = packages.iter().map(|s| short_name(s)).collect();

    // Sort requests into version-solver roots (semver constraints, or
    // unconstrained) and direct tag installs (explicit non-semver tags, or
    // repositories without semver tags). Direct installs contribute their
    // requirements to the solver.
    let probe = Provider::new(cfg, &client, Vec::new());
    let mut root_deps = Vec::new();
    let mut plan: Vec<(String, Resolved)> = Vec::new();
    let mut direct_tag = |spec: &str, tag: &str| -> Result<Vec<_>> {
        let (name, _) = parse_spec(spec);
        let resolved = client
            .resolve(&cfg.repo_for(&name), tag)
            .map_err(|e| anyhow!("{spec}: {e:#}"))?;
        let mut deps = Vec::new();
        for req in requirements(&resolved) {
            let (dep, c) = parse_requirement(&req);
            deps.push((dep, parse_range(c.as_deref())?));
        }
        plan.push((spec.to_string(), resolved));
        Ok(deps)
    };
    for spec in &packages {
        let (name, constraint) = parse_requirement(spec);
        match constraint.as_deref() {
            None if probe.versions_of(&name)?.is_empty() => {
                root_deps.extend(direct_tag(spec, "latest")?);
            }
            c => match parse_range(c) {
                Ok(range) => root_deps.push((name, range)),
                // Not a semver constraint: treat it as a literal tag.
                Err(_) => root_deps.extend(direct_tag(spec, c.unwrap())?),
            },
        }
    }

    // Solve version constraints across the whole dependency graph.
    let provider = Provider::new(cfg, &client, root_deps);
    let direct: HashSet<String> = plan.iter().map(|(s, _)| short_name(s)).collect();
    for (name, version) in resolve::solve(&provider)? {
        if direct.contains(&short_name(&name)) {
            continue;
        }
        let resolved = match provider.take_resolved(&name, &version) {
            Some(r) => r,
            None => client.resolve(&cfg.repo_for(&name), &version)?,
        };
        plan.push((format!("{name}@{version}"), resolved));
    }

    // --build-from-source: use the published source for requested packages.
    if from_source {
        for (spec, resolved) in &mut plan {
            if requested.contains(&short_name(spec)) {
                let (name, tag) = parse_spec(spec);
                *resolved = client
                    .resolve_source(&cfg.repo_for(&name), &tag)
                    .map_err(|_| anyhow!("{spec} has no published source to build from"))?;
            }
        }
    }

    let failures = std::thread::scope(|s| {
        let handles: Vec<_> = plan
            .iter()
            .map(|(spec, resolved)| {
                let client = &client;
                s.spawn(move || {
                    install_one(cfg, client, spec, resolved, force).map_err(|e| (spec.clone(), e))
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("install thread panicked").err())
            .map(|(spec, e)| eprintln!("error: {spec}: {e:#}"))
            .count()
    });
    if failures > 0 {
        bail!("{failures} package(s) failed to install");
    }
    println!("Done in {:.2}s", start.elapsed().as_secs_f64());
    Ok(())
}

/// Fetch and verify the signature artifact for `resolved`, if a verify key
/// is configured. Fails closed: a configured key plus a missing or invalid
/// signature aborts the install.
fn verify_signature(cfg: &Config, client: &Client, repo: &str, resolved: &Resolved) -> Result<()> {
    let Some(trust_root) = cfg.verify_key() else {
        return Ok(());
    };
    let check = check_signature(client, repo, resolved, &trust_root)?;
    println!(
        "Verified {repo} ({}) with {}",
        resolved.root_digest,
        check.key.display()
    );
    Ok(())
}

/// Outcome of a successful signature check.
struct SignatureCheck {
    /// Trusted key that matched.
    key: std::path::PathBuf,
    /// The verified base64 signature.
    signature_b64: String,
    /// sha256 (hex) of the signed simple-signing payload.
    payload_sha256: String,
    /// Rekor receipt stored with the signature, if any.
    rekor: Option<crate::rekor::Entry>,
}

/// Verify the cosign-format signature artifact for `resolved` against the
/// keys in `trust_root`.
fn check_signature(
    client: &Client,
    repo: &str,
    resolved: &Resolved,
    trust_root: &std::path::Path,
) -> Result<SignatureCheck> {
    let trusted = sign::load_trusted_keys(trust_root)?;
    let tag = sign::sig_tag(&resolved.root_digest);
    let sig = client
        .resolve(repo, &tag)
        .map_err(|_| anyhow!("no signature found for {repo} ({})", resolved.root_digest))?;
    let mut last_err = anyhow!("signature artifact for {repo} has no signatures");
    for layer in &sig.manifest.layers {
        let Some(signature_b64) = layer
            .annotations
            .as_ref()
            .and_then(|a| a.get(sign::ANNOTATION_SIGNATURE))
        else {
            continue;
        };
        let tmp = std::env::temp_dir().join(format!(
            "pkgoci-sig-{}-{}",
            std::process::id(),
            &layer.digest.trim_start_matches("sha256:")[..12]
        ));
        client.download_blob(repo, &tag, layer, &tmp)?;
        let payload_bytes = std::fs::read(&tmp)?;
        let _ = std::fs::remove_file(&tmp);

        // The signed payload must pin the digest we resolved.
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes)?;
        let signed_digest = payload
            .pointer("/critical/image/docker-manifest-digest")
            .and_then(|d| d.as_str())
            .unwrap_or_default();
        if signed_digest != resolved.root_digest {
            last_err = anyhow!(
                "signature is for {signed_digest}, not {}",
                resolved.root_digest
            );
            continue;
        }
        match sign::verify(&trusted, &payload_bytes, signature_b64) {
            Ok(key) => {
                let rekor = layer
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get(oci::ANNOTATION_REKOR))
                    .map(|j| serde_json::from_str(j))
                    .transpose()
                    .context("parsing rekor receipt annotation")?;
                return Ok(SignatureCheck {
                    key,
                    signature_b64: signature_b64.clone(),
                    payload_sha256: oci::sha256_hex(&payload_bytes),
                    rekor,
                });
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn install_one(
    cfg: &Config,
    client: &Client,
    spec: &str,
    resolved: &Resolved,
    force: bool,
) -> Result<()> {
    let (name, tag) = parse_spec(spec);
    let repo = cfg.repo_for(&name);
    let short = short_name(spec);

    let version = resolved.version(&tag);

    if !force && cellar::read_receipt(cfg, &short, &version).is_some() {
        println!("{short} {version} is already installed");
        return Ok(());
    }

    verify_signature(cfg, client, &repo, resolved)?;

    let layer = resolved
        .manifest
        .layers
        .first()
        .ok_or_else(|| anyhow!("manifest for {repo}:{tag} has no layers"))?
        .clone();

    // Download (or reuse the cached, digest-verified archive).
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
        println!(
            "Downloading {short} {version} ({})...",
            cellar::human_bytes(layer.size)
        );
        client.download_blob(&repo, &tag, &layer, &cache_file)?;
    }

    // Extract into the keg (building from source first when needed).
    let keg = cellar::keg_path(cfg, &short, &version);
    if keg.exists() {
        // Unlink a previous install of the same version before replacing it.
        if let Some(old) = cellar::read_receipt(cfg, &short, &version) {
            cellar::unlink(cfg, &old);
        }
        std::fs::remove_dir_all(&keg)?;
    }
    if resolved.from_source {
        println!("Building {short} {version} from source...");
        let build_dir = cfg
            .cache()
            .join(format!("build-{short}-{version}-{}", std::process::id()));
        if build_dir.exists() {
            std::fs::remove_dir_all(&build_dir)?;
        }
        extract::extract_layer(&cache_file, &layer.media_type, &build_dir)?;
        let steps: Vec<crate::pkgocifile::Step> = serde_json::from_str(
            resolved
                .manifest
                .annotation(oci::ANNOTATION_BUILD)
                .unwrap_or("[]"),
        )
        .context("parsing build steps annotation")?;
        if steps.is_empty() {
            bail!("source manifest for {short} has no build steps");
        }
        let image = resolved
            .manifest
            .annotation(oci::ANNOTATION_IMAGE)
            .unwrap_or(crate::sandbox::DEFAULT_IMAGE);
        let env: Vec<(String, String)> =
            serde_json::from_str::<std::collections::BTreeMap<String, String>>(
                resolved
                    .manifest
                    .annotation(oci::ANNOTATION_ENV)
                    .unwrap_or("{}"),
            )
            .context("parsing build env annotation")?
            .into_iter()
            .collect();
        println!("Sandbox: {}", crate::sandbox::describe(image));
        run_steps(&steps, "RUN", &build_dir, &short, &version, image, &env)?;
        let tests: Vec<crate::pkgocifile::Step> = serde_json::from_str(
            resolved
                .manifest
                .annotation(oci::ANNOTATION_TEST)
                .unwrap_or("[]"),
        )
        .context("parsing test steps annotation")?;
        run_steps(&tests, "TEST", &build_dir, &short, &version, image, &env)?;
        let output = build_dir.join(
            resolved
                .manifest
                .annotation(oci::ANNOTATION_OUTPUT)
                .unwrap_or("out"),
        );
        if !output.is_dir() {
            bail!(
                "build did not produce output directory {}",
                output.display()
            );
        }
        copy_tree(&output, &keg)?;
        std::fs::remove_dir_all(&build_dir)?;
    } else {
        extract::extract_layer(&cache_file, &layer.media_type, &keg)?;
    }

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
            dependencies: requirements(resolved)
                .iter()
                .map(|d| short_name(d))
                .collect(),
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

pub fn uninstall(cfg: &Config, packages: Vec<String>, force: bool) -> Result<()> {
    if packages.is_empty() {
        bail!("no packages given");
    }
    let removing: HashSet<String> = packages.iter().map(|s| short_name(s)).collect();
    for spec in packages {
        let short = short_name(&spec);
        let versions = cellar::installed_versions(cfg, &short);
        if versions.is_empty() {
            eprintln!("error: {short} is not installed");
            continue;
        }
        if !force {
            // Refuse to remove something another installed package requires.
            let dependents: Vec<String> = cellar::list_installed(cfg)
                .into_iter()
                .filter(|(name, _)| *name != short && !removing.contains(name))
                .filter(|(name, versions)| {
                    versions
                        .last()
                        .and_then(|v| cellar::read_receipt(cfg, name, v))
                        .is_some_and(|r| r.dependencies.contains(&short))
                })
                .map(|(name, _)| name)
                .collect();
            if !dependents.is_empty() {
                eprintln!(
                    "error: {short} is required by {} (use --force to remove anyway)",
                    dependents.join(", ")
                );
                continue;
            }
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

pub fn info(cfg: &Config, package: String) -> Result<()> {
    let (name, tag) = parse_spec(&package);
    let repo = cfg.repo_for(&name);
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let client = Client::new(&cfg.registry);
    let resolved = client.resolve(&repo, &tag)?;
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
    let deps = requirements(&resolved);
    if !deps.is_empty() {
        println!("Requires: {}", deps.join(", "));
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

pub fn search(cfg: &Config, term: String) -> Result<()> {
    if !cfg.is_docker_hub() {
        bail!(
            "search is only supported on Docker Hub (registry: {})",
            cfg.registry
        );
    }
    let mut url = format!(
        "https://hub.docker.com/v2/repositories/{}/?page_size=100",
        cfg.namespace
    );
    let mut found = 0;
    loop {
        let v: serde_json::Value = ureq::get(&url)
            .call()
            .with_context(|| format!("listing repositories under {}", cfg.namespace))?
            .into_json()?;
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

pub fn upgrade(cfg: &Config, packages: Vec<String>) -> Result<()> {
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

    let client = Client::new(&cfg.registry);
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
        let resolved = client.resolve(&repo, "latest")?;
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
        false,
    )?;
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

// ------------------------------------------------------------------- build

/// Build a package described by a Pkgocifile into the local store as a
/// standard OCI image layout (like `docker build`).
pub fn build(
    cfg: &Config,
    path: std::path::PathBuf,
    file: Option<std::path::PathBuf>,
) -> Result<()> {
    let started = rfc3339_now();
    let pkgocifile = file.unwrap_or_else(|| path.join(crate::pkgocifile::FILE_NAME));
    let mut spec = crate::pkgocifile::parse(&pkgocifile)?;
    let context = pkgocifile
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();

    // The context stays read-only (like docker build): fetching and building
    // happen in a scratch copy.
    let work = if spec.run.is_empty() {
        context.clone()
    } else {
        let staging = cfg
            .cache()
            .join(format!("build-{}-{}", spec.name, spec.version));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        copy_tree(&context, &staging)?;
        staging
    };

    // Fetch digest-pinned upstream sources into the work tree.
    for (url, sha) in &spec.fetches {
        fetch_source(cfg, url, sha, &spec, &work)?;
    }

    // Pack the (pristine, post-fetch, pre-build) source tree now so the
    // published source layer never contains build artifacts.
    let source_bytes = spec
        .source
        .as_ref()
        .map(|rel| extract::pack_dir(&work.join(rel)))
        .transpose()?;

    // Execute RUN steps sandboxed and pack their OUTPUT for the host
    // platform (like docker build's RUN), then run TEST checks.
    if !spec.run.is_empty() {
        println!("Sandbox: {}", crate::sandbox::describe(&spec.image));
        run_steps(
            &spec.run,
            "RUN",
            &work,
            &spec.name,
            &spec.version,
            &spec.image,
            &spec.env,
        )?;
        run_steps(
            &spec.tests,
            "TEST",
            &work,
            &spec.name,
            &spec.version,
            &spec.image,
            &spec.env,
        )?;
        let output = work.join(&spec.output);
        if !output.is_dir() {
            bail!(
                "RUN steps did not produce OUTPUT directory {}",
                output.display()
            );
        }
        let (os, arch) = (
            crate::platform::os().to_string(),
            crate::platform::arch().to_string(),
        );
        spec.platforms.retain(|(o, a, _)| (o, a) != (&os, &arch));
        spec.platforms.push((os, arch, output));
    }

    let out = cfg.store().join(&spec.name).join(&spec.version);
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    let blobs = out.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)?;
    std::fs::write(out.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let mut annotations = oci::Annotations::new();
    annotations.insert(oci::ANNOTATION_VERSION.into(), spec.version.clone());
    if let Some(d) = &spec.description {
        annotations.insert(oci::ANNOTATION_DESCRIPTION.into(), d.clone());
    }
    if let Some(l) = &spec.license {
        annotations.insert(oci::ANNOTATION_LICENSES.into(), l.clone());
    }
    if let Some(u) = &spec.url {
        annotations.insert(oci::ANNOTATION_URL.into(), u.clone());
    }
    if !spec.requires.is_empty() {
        annotations.insert(oci::ANNOTATION_REQUIRES.into(), spec.requires.join(","));
    }

    // Shared empty config blob.
    let config_digest = write_blob(&blobs, b"{}")?;

    let mut manifests = Vec::new();
    for (os, arch, dir) in &spec.platforms {
        let layer_bytes = extract::pack_dir(dir)?;
        let layer_size = layer_bytes.len() as u64;
        let layer_digest = write_blob(&blobs, &layer_bytes)?;
        println!("Packed {os}/{arch} ({})", cellar::human_bytes(layer_size));

        let manifest = oci::Manifest {
            schema_version: 2,
            media_type: Some(oci::MT_OCI_MANIFEST.into()),
            config: oci::Descriptor {
                media_type: oci::MT_OCI_CONFIG.into(),
                digest: config_digest.clone(),
                size: 2,
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
        let manifest_json = serde_json::to_vec(&manifest)?;
        let manifest_digest = write_blob(&blobs, &manifest_json)?;
        manifests.push(oci::Descriptor {
            media_type: oci::MT_OCI_MANIFEST.into(),
            digest: manifest_digest,
            size: manifest_json.len() as u64,
            platform: Some(oci::Platform {
                os: os.clone(),
                architecture: arch.clone(),
            }),
            annotations: None,
        });
    }

    let platforms = manifests.len();

    // Publish the source tree (plus its build recipe) so users on platforms
    // without prebuilt binaries can build from source at install time.
    let mut source_digest_hex = None;
    if let Some(src_bytes) = source_bytes {
        let src_size = src_bytes.len() as u64;
        let src_digest = write_blob(&blobs, &src_bytes)?;
        source_digest_hex = Some(src_digest.trim_start_matches("sha256:").to_string());
        println!("Packed source ({})", cellar::human_bytes(src_size));

        let mut src_annotations = annotations.clone();
        src_annotations.insert(
            oci::ANNOTATION_BUILD.into(),
            serde_json::to_string(&spec.run)?,
        );
        src_annotations.insert(oci::ANNOTATION_OUTPUT.into(), spec.output.clone());
        src_annotations.insert(oci::ANNOTATION_IMAGE.into(), spec.image.clone());
        if !spec.tests.is_empty() {
            src_annotations.insert(
                oci::ANNOTATION_TEST.into(),
                serde_json::to_string(&spec.tests)?,
            );
        }
        if !spec.env.is_empty() {
            let env: std::collections::BTreeMap<_, _> = spec.env.iter().cloned().collect();
            src_annotations.insert(oci::ANNOTATION_ENV.into(), serde_json::to_string(&env)?);
        }
        let manifest = oci::Manifest {
            schema_version: 2,
            media_type: Some(oci::MT_OCI_MANIFEST.into()),
            config: oci::Descriptor {
                media_type: oci::MT_OCI_CONFIG.into(),
                digest: config_digest.clone(),
                size: 2,
                platform: None,
                annotations: None,
            },
            layers: vec![oci::Descriptor {
                media_type: oci::MT_LAYER_TAR_GZIP.into(),
                digest: src_digest,
                size: src_size,
                platform: None,
                annotations: None,
            }],
            annotations: Some(src_annotations),
        };
        let manifest_json = serde_json::to_vec(&manifest)?;
        let manifest_digest = write_blob(&blobs, &manifest_json)?;
        manifests.push(oci::Descriptor {
            media_type: oci::MT_OCI_MANIFEST.into(),
            digest: manifest_digest,
            size: manifest_json.len() as u64,
            platform: Some(oci::Platform {
                os: oci::SOURCE_OS.into(),
                architecture: oci::SOURCE_ARCH.into(),
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
    let index_json = serde_json::to_vec(&index)?;
    let index_digest = write_blob(&blobs, &index_json)?;

    // OCI layout entrypoint, tagged with the version.
    let mut ref_annotations = oci::Annotations::new();
    ref_annotations.insert(
        "org.opencontainers.image.ref.name".into(),
        spec.version.clone(),
    );
    let layout_index = oci::Index {
        schema_version: 2,
        media_type: Some(oci::MT_OCI_INDEX.into()),
        manifests: vec![oci::Descriptor {
            media_type: oci::MT_OCI_INDEX.into(),
            digest: index_digest.clone(),
            size: index_json.len() as u64,
            platform: None,
            annotations: Some(ref_annotations),
        }],
        annotations: None,
    };
    std::fs::write(out.join("index.json"), serde_json::to_vec(&layout_index)?)?;

    // SLSA v1 build provenance for the package, signed and pushed as a DSSE
    // attestation by `pkgoci push --sign`.
    let pkgocifile_bytes = std::fs::read(&pkgocifile)?;
    let mut materials = vec![serde_json::json!({
        "name": crate::pkgocifile::FILE_NAME,
        "digest": {"sha256": oci::sha256_hex(&pkgocifile_bytes)}
    })];
    if let Some(hex) = source_digest_hex {
        materials.push(serde_json::json!({"name": "source", "digest": {"sha256": hex}}));
    }
    for (url, sha) in &spec.fetches {
        materials.push(serde_json::json!({
            "name": substitute(url, &spec),
            "digest": {"sha256": sha}
        }));
    }
    let provenance = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{
            "name": spec.name,
            "digest": {"sha256": index_digest.trim_start_matches("sha256:")}
        }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": "https://pkgoci.dev/Pkgocifile/v1",
                "externalParameters": {
                    "name": spec.name,
                    "version": spec.version,
                    "run": spec.run,
                    "requires": spec.requires,
                },
                "resolvedDependencies": materials,
            },
            "runDetails": {
                "builder": {"id": concat!("https://pkgoci.dev/pkgoci/", env!("CARGO_PKG_VERSION"))},
                "metadata": {
                    "invocationId": format!("{}-{}", cellar::now_unix(), std::process::id()),
                    "startedOn": started,
                    "finishedOn": rfc3339_now(),
                },
            },
        },
    });
    std::fs::write(
        out.join("provenance.json"),
        serde_json::to_vec(&provenance)?,
    )?;

    if work != context {
        let _ = std::fs::remove_dir_all(&work);
    }
    println!(
        "Built {} {} ({platforms} platform(s), {index_digest})",
        spec.name, spec.version
    );
    println!("Push it with: pkgoci push {}@{}", spec.name, spec.version);
    Ok(())
}

/// Recursively copy a directory tree (permissions preserved by fs::copy).
fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Substitute `${PKGOCI_NAME}`/`${PKGOCI_VERSION}` in FETCH urls.
fn substitute(url: &str, spec: &crate::pkgocifile::Spec) -> String {
    url.replace("${PKGOCI_NAME}", &spec.name)
        .replace("${PKGOCI_VERSION}", &spec.version)
}

/// Download a digest-pinned source tarball (cached by digest) and extract it
/// into the build context with the leading path component stripped.
fn fetch_source(
    cfg: &Config,
    url: &str,
    sha256: &str,
    spec: &crate::pkgocifile::Spec,
    context: &std::path::Path,
) -> Result<()> {
    use std::io::Read;
    let url = substitute(url, spec);
    std::fs::create_dir_all(cfg.cache())?;
    let cached = cfg.cache().join(format!("fetch-{sha256}.tar.gz"));
    let bytes = if cached.exists() {
        std::fs::read(&cached)?
    } else {
        println!("FETCH {url}");
        let mut bytes = Vec::new();
        ureq::get(&url)
            .call()
            .with_context(|| format!("fetching {url}"))?
            .into_reader()
            .read_to_end(&mut bytes)?;
        std::fs::write(&cached, &bytes)?;
        bytes
    };
    let got = oci::sha256_hex(&bytes);
    if got != sha256 {
        let _ = std::fs::remove_file(&cached);
        bail!("digest mismatch for {url}: expected {sha256}, got {got}");
    }
    extract::extract_tar_gz_strip1(&bytes, context)
        .with_context(|| format!("extracting {url} into {}", context.display()))
}

/// Run Pkgocifile RUN/TEST steps in `dir` inside the platform sandbox,
/// skipping steps limited to other OSes.
fn run_steps(
    steps: &[crate::pkgocifile::Step],
    label: &str,
    dir: &std::path::Path,
    name: &str,
    version: &str,
    image: &str,
    env: &[(String, String)],
) -> Result<()> {
    let mut full_env = vec![
        ("PKGOCI_NAME".to_string(), name.to_string()),
        ("PKGOCI_VERSION".to_string(), version.to_string()),
        ("PKGOCI_OS".to_string(), crate::platform::os().to_string()),
        (
            "PKGOCI_ARCH".to_string(),
            crate::platform::arch().to_string(),
        ),
    ];
    full_env.extend(env.iter().cloned());
    for step in steps.iter().filter(|s| s.applies_to(crate::platform::os())) {
        println!("{label} {}", step.cmd);
        let status = crate::sandbox::command(&step.cmd, dir, image, &full_env)?
            .status()
            .with_context(|| format!("running {:?}", step.cmd))?;
        if !status.success() {
            bail!("{label} {:?} failed with {status}", step.cmd);
        }
    }
    Ok(())
}

/// RFC 3339 UTC timestamp without external crates.
fn rfc3339_now() -> String {
    let secs = cellar::now_unix() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn write_blob(blobs: &std::path::Path, bytes: &[u8]) -> Result<String> {
    let hex = oci::sha256_hex(bytes);
    std::fs::write(blobs.join(&hex), bytes)?;
    Ok(format!("sha256:{hex}"))
}

fn read_blob(dir: &std::path::Path, digest: &str) -> Result<Vec<u8>> {
    let path = dir
        .join("blobs")
        .join("sha256")
        .join(digest.trim_start_matches("sha256:"));
    let bytes = std::fs::read(&path).with_context(|| format!("reading blob {}", path.display()))?;
    if format!("sha256:{}", oci::sha256_hex(&bytes)) != digest {
        bail!("corrupt blob in build store: {}", path.display());
    }
    Ok(bytes)
}

// ------------------------------------------------------------------- push

/// Push a built package from the local store to the registry
/// (like `docker push`).
pub fn push(cfg: &Config, package: String, sign_it: bool, rekor_it: bool) -> Result<()> {
    if rekor_it && !sign_it {
        bail!("--rekor requires --sign");
    }
    let (name, version) = parse_requirement(&package);
    let short = short_name(&package);
    let version = match version {
        Some(v) => v,
        None => {
            // Newest built version.
            let mut versions: Vec<semver::Version> = std::fs::read_dir(cfg.store().join(&short))
                .map_err(|_| anyhow!("no built package {short} (run `pkgoci build` first)"))?
                .flatten()
                .filter_map(|e| semver::Version::parse(&e.file_name().to_string_lossy()).ok())
                .collect();
            versions.sort();
            versions
                .pop()
                .ok_or_else(|| anyhow!("no built versions of {short} in the store"))?
                .to_string()
        }
    };
    let dir = cfg.store().join(&short).join(&version);
    if !dir.exists() {
        bail!("{short} {version} is not built (run `pkgoci build` first)");
    }

    // Walk the layout: layout index -> package index -> platform manifests.
    let layout_index: oci::Index = serde_json::from_slice(&std::fs::read(dir.join("index.json"))?)?;
    let top = layout_index
        .manifests
        .first()
        .ok_or_else(|| anyhow!("empty index.json in {}", dir.display()))?;
    let index_bytes = read_blob(&dir, &top.digest)?;
    let index: oci::Index = serde_json::from_slice(&index_bytes)?;

    let repo = cfg.repo_for(&name);
    let client = Client::new(&cfg.registry);
    for desc in &index.manifests {
        let manifest_bytes = read_blob(&dir, &desc.digest)?;
        let manifest: oci::Manifest = serde_json::from_slice(&manifest_bytes)?;
        let platform = desc
            .platform
            .as_ref()
            .map(|p| format!("{}/{}", p.os, p.architecture))
            .unwrap_or_default();
        println!(
            "Pushing {platform} ({})...",
            cellar::human_bytes(manifest.layers.iter().map(|l| l.size).sum())
        );
        for blob in std::iter::once(&manifest.config).chain(&manifest.layers) {
            let path = dir
                .join("blobs")
                .join("sha256")
                .join(blob.digest.trim_start_matches("sha256:"));
            client.push_blob(&repo, &version, &blob.digest, &path)?;
        }
        client.push_manifest(
            &repo,
            &desc.digest,
            oci::MT_OCI_MANIFEST,
            std::str::from_utf8(&manifest_bytes)?,
        )?;
    }
    let index_json = std::str::from_utf8(&index_bytes)?;
    for tag in [version.as_str(), "latest"] {
        client.push_manifest(&repo, tag, oci::MT_OCI_INDEX, index_json)?;
    }

    if sign_it {
        // Cosign-compatible signature: simple-signing payload as the layer
        // blob, base64 signature in the cosign annotation, stored under the
        // sha256-<digest>.sig tag. Verifiable with stock cosign.
        let index_digest = &top.digest;
        let image_ref = format!("{}/{repo}", cfg.registry);
        let payload_bytes = sign::payload(&image_ref, index_digest);
        let signature_b64 = sign::sign(&cfg.signing_key(), &payload_bytes)?;
        let payload_digest = format!("sha256:{}", oci::sha256_hex(&payload_bytes));
        let payload_path = std::env::temp_dir().join(format!("pkgoci-sig-{}", std::process::id()));
        std::fs::write(&payload_path, &payload_bytes)?;
        client.push_blob(&repo, &version, &payload_digest, &payload_path)?;
        let _ = std::fs::remove_file(&payload_path);

        let mut sig_annotations = oci::Annotations::new();
        sig_annotations.insert(sign::ANNOTATION_SIGNATURE.into(), signature_b64.clone());

        // Record the signature in the Rekor transparency log and store the
        // receipt with the signature.
        if rekor_it {
            let rekor_url = crate::rekor::url();
            let entry = crate::rekor::upload(
                &rekor_url,
                &payload_bytes,
                &signature_b64,
                &sign::public_key_pem(&cfg.signing_key())?,
            )?;
            println!(
                "Recorded in transparency log: {} (index {}, {})",
                rekor_url, entry.log_index, entry.uuid
            );
            sig_annotations.insert(oci::ANNOTATION_REKOR.into(), serde_json::to_string(&entry)?);
        }
        let sig_manifest = oci::Manifest {
            schema_version: 2,
            media_type: Some(oci::MT_OCI_MANIFEST.into()),
            config: oci::Descriptor {
                media_type: oci::MT_OCI_CONFIG.into(),
                digest: format!("sha256:{}", oci::sha256_hex(b"{}")),
                size: 2,
                platform: None,
                annotations: None,
            },
            layers: vec![oci::Descriptor {
                media_type: sign::MT_SIMPLE_SIGNING.into(),
                digest: payload_digest,
                size: payload_bytes.len() as u64,
                platform: None,
                annotations: Some(sig_annotations),
            }],
            annotations: None,
        };
        client.push_manifest(
            &repo,
            &sign::sig_tag(index_digest),
            oci::MT_OCI_MANIFEST,
            &serde_json::to_string(&sig_manifest)?,
        )?;
        println!("Signed {index_digest} with {}", cfg.signing_key().display());

        // Build provenance (SLSA v1) as a cosign-compatible DSSE attestation.
        let provenance_path = dir.join("provenance.json");
        if provenance_path.exists() {
            let statement = std::fs::read(&provenance_path)?;
            let envelope = sign::dsse_envelope(&cfg.signing_key(), &statement)?;
            let envelope_digest = format!("sha256:{}", oci::sha256_hex(&envelope));
            let envelope_path =
                std::env::temp_dir().join(format!("pkgoci-att-{}", std::process::id()));
            std::fs::write(&envelope_path, &envelope)?;
            client.push_blob(&repo, &version, &envelope_digest, &envelope_path)?;
            let _ = std::fs::remove_file(&envelope_path);

            let att_manifest = oci::Manifest {
                schema_version: 2,
                media_type: Some(oci::MT_OCI_MANIFEST.into()),
                config: oci::Descriptor {
                    media_type: oci::MT_OCI_CONFIG.into(),
                    digest: format!("sha256:{}", oci::sha256_hex(b"{}")),
                    size: 2,
                    platform: None,
                    annotations: None,
                },
                layers: vec![oci::Descriptor {
                    media_type: sign::MT_DSSE_ENVELOPE.into(),
                    digest: envelope_digest,
                    size: envelope.len() as u64,
                    platform: None,
                    // The DSSE envelope carries the signature; cosign still
                    // requires the annotation key to be present.
                    annotations: Some(
                        [(sign::ANNOTATION_SIGNATURE.to_string(), String::new())].into(),
                    ),
                }],
                annotations: None,
            };
            client.push_manifest(
                &repo,
                &sign::att_tag(index_digest),
                oci::MT_OCI_MANIFEST,
                &serde_json::to_string(&att_manifest)?,
            )?;
            println!("Attested build provenance for {index_digest}");
        }
    }

    println!(
        "Pushed {}/{repo}:{version} ({} platform(s))",
        cfg.registry,
        index.manifests.len()
    );
    Ok(())
}

// ----------------------------------------------------------------- verify

/// Verify a package's signature explicitly (like `brew verify`).
pub fn verify(cfg: &Config, package: String, key: Option<std::path::PathBuf>) -> Result<()> {
    let trust_root = key
        .or_else(|| cfg.verify_key())
        .ok_or_else(|| anyhow!("no key given (use --key or set PKGOCI_VERIFY_KEY)"))?;
    let (name, tag) = parse_spec(&package);
    let repo = cfg.repo_for(&name);
    let client = Client::new(&cfg.registry);
    let resolved = client.resolve(&repo, &tag)?;
    let check = check_signature(&client, &repo, &resolved, &trust_root)?;
    println!(
        "OK: {}/{repo}:{tag} ({}) verified with {}",
        cfg.registry,
        resolved.root_digest,
        check.key.display()
    );

    // Transparency log receipt (optional but reported and verified).
    match &check.rekor {
        None => println!("No transparency log receipt."),
        Some(entry) => {
            crate::rekor::verify(entry, &check.signature_b64, &check.payload_sha256)?;
            println!(
                "OK: transparency log entry {} (index {}) at {} verified",
                entry.uuid, entry.log_index, entry.url
            );
        }
    }

    // Build provenance attestation (optional but reported).
    match client.resolve(&repo, &sign::att_tag(&resolved.root_digest)) {
        Err(_) => println!("No build provenance attestation."),
        Ok(att) => {
            let trusted = sign::load_trusted_keys(&trust_root)?;
            let layer = att
                .manifest
                .layers
                .first()
                .ok_or_else(|| anyhow!("malformed attestation artifact"))?;
            let tmp = std::env::temp_dir().join(format!("pkgoci-att-{}", std::process::id()));
            client.download_blob(&repo, &sign::att_tag(&resolved.root_digest), layer, &tmp)?;
            let envelope = std::fs::read(&tmp)?;
            let _ = std::fs::remove_file(&tmp);
            let (statement, key) = sign::verify_dsse(&trusted, &envelope)?;
            let subject = statement
                .pointer("/subject/0/digest/sha256")
                .and_then(|d| d.as_str())
                .unwrap_or_default();
            if format!("sha256:{subject}") != resolved.root_digest {
                bail!(
                    "attestation subject sha256:{subject} does not match {}",
                    resolved.root_digest
                );
            }
            println!(
                "OK: build provenance ({}) by {} at {}, verified with {}",
                statement["predicateType"].as_str().unwrap_or("?"),
                statement
                    .pointer("/predicate/runDetails/builder/id")
                    .and_then(|b| b.as_str())
                    .unwrap_or("?"),
                statement
                    .pointer("/predicate/runDetails/metadata/finishedOn")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?"),
                key.display()
            );
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- keygen

pub fn keygen(cfg: &Config, out: Option<std::path::PathBuf>) -> Result<()> {
    let dir = out.unwrap_or_else(|| cfg.prefix.join("keys"));
    let (key, public) = sign::generate(&dir)?;
    println!(
        "Private key: {} (keep secret; used by `pkgoci push --sign`)",
        key.display()
    );
    println!(
        "Public key:  {} (distribute; set PKGOCI_VERIFY_KEY to enforce)",
        public.display()
    );
    Ok(())
}
