//! Version solving with the PubGrub algorithm.
//!
//! Requirements are semver ranges (`libfoo@^1.2`, `libfoo@>=1,<3`, exact
//! `libfoo@1.2.3`, or bare `libfoo` for any version). Available versions are
//! the repository's semver tags; each candidate's own requirements come from
//! its `dev.pkgoci.requires` annotation. Conflicts fail with PubGrub's
//! derivation-tree explanation.

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use pubgrub::range::Range;
use pubgrub::report::{DefaultStringReporter, Reporter};
use pubgrub::solver::{
    choose_package_with_fewest_versions, resolve, Dependencies, DependencyProvider,
};
use pubgrub::type_aliases::Map;
use pubgrub::version::SemanticVersion;

use crate::config::Config;
use crate::oci;
use crate::registry::{Client, Resolved};

const ROOT: &str = "(command line)";

/// Split `name@constraint` into (name, Option<constraint>).
pub fn parse_requirement(spec: &str) -> (String, Option<String>) {
    match spec.rsplit_once('@') {
        Some((name, c)) if !c.is_empty() && !c.contains('/') => {
            (name.to_string(), Some(c.to_string()))
        }
        _ => (spec.to_string(), None),
    }
}

/// Parse a requirement constraint into a PubGrub range.
/// Bare exact versions (`1.2.3`) pin exactly; anything else is a semver
/// range (`^1.2`, `~1.2.3`, `>=1,<3`, `1.*`).
pub fn parse_range(constraint: Option<&str>) -> Result<Range<SemanticVersion>> {
    let Some(c) = constraint else {
        return Ok(Range::any());
    };
    if let Ok(v) = semver::Version::parse(c) {
        if v.pre.is_empty() && v.build.is_empty() {
            return Ok(Range::exact(to_pg(&v)));
        }
    }
    let req = semver::VersionReq::parse(c)
        .map_err(|e| anyhow!("invalid version requirement {c:?}: {e}"))?;
    let mut range = Range::any();
    for comp in &req.comparators {
        range = range.intersection(&comparator_range(comp)?);
    }
    Ok(range)
}

fn to_pg(v: &semver::Version) -> SemanticVersion {
    SemanticVersion::new(v.major as u32, v.minor as u32, v.patch as u32)
}

fn sv(major: u64, minor: u64, patch: u64) -> SemanticVersion {
    SemanticVersion::new(major as u32, minor as u32, patch as u32)
}

fn comparator_range(c: &semver::Comparator) -> Result<Range<SemanticVersion>> {
    use semver::Op;
    let (major, minor, patch) = (c.major, c.minor, c.patch);
    let full = |d: u64| sv(major, minor.unwrap_or(0), patch.unwrap_or(0) + d);
    Ok(match (c.op, minor, patch) {
        (Op::Exact, Some(n), Some(p)) => Range::exact(sv(major, n, p)),
        (Op::Exact, Some(n), None) => Range::between(sv(major, n, 0), sv(major, n + 1, 0)),
        (Op::Exact, None, _) => Range::between(sv(major, 0, 0), sv(major + 1, 0, 0)),
        (Op::Greater, Some(n), Some(p)) => Range::higher_than(sv(major, n, p + 1)),
        (Op::Greater, Some(n), None) => Range::higher_than(sv(major, n + 1, 0)),
        (Op::Greater, None, _) => Range::higher_than(sv(major + 1, 0, 0)),
        (Op::GreaterEq, _, _) => Range::higher_than(full(0)),
        (Op::Less, _, _) => Range::strictly_lower_than(full(0)),
        (Op::LessEq, Some(n), Some(p)) => Range::strictly_lower_than(sv(major, n, p + 1)),
        (Op::LessEq, Some(n), None) => Range::strictly_lower_than(sv(major, n + 1, 0)),
        (Op::LessEq, None, _) => Range::strictly_lower_than(sv(major + 1, 0, 0)),
        (Op::Tilde, Some(n), Some(p)) => Range::between(sv(major, n, p), sv(major, n + 1, 0)),
        (Op::Tilde, Some(n), None) => Range::between(sv(major, n, 0), sv(major, n + 1, 0)),
        (Op::Tilde, None, _) => Range::between(sv(major, 0, 0), sv(major + 1, 0, 0)),
        (Op::Caret, _, _) => caret_range(major, minor, patch),
        (Op::Wildcard, Some(n), _) => Range::between(sv(major, n, 0), sv(major, n + 1, 0)),
        (Op::Wildcard, None, _) => Range::between(sv(major, 0, 0), sv(major + 1, 0, 0)),
        (op, _, _) => bail!("unsupported version operator {op:?}"),
    })
}

fn caret_range(major: u64, minor: Option<u64>, patch: Option<u64>) -> Range<SemanticVersion> {
    match (major, minor, patch) {
        (0, Some(0), Some(p)) => Range::exact(sv(0, 0, p)),
        (0, Some(n), p) if n > 0 => Range::between(sv(0, n, p.unwrap_or(0)), sv(0, n + 1, 0)),
        (m, n, p) => Range::between(sv(m, n.unwrap_or(0), p.unwrap_or(0)), sv(m + 1, 0, 0)),
    }
}

/// Registry-backed dependency provider with per-command caches.
pub struct Provider<'a> {
    cfg: &'a Config,
    client: &'a Client,
    root_deps: Vec<(String, Range<SemanticVersion>)>,
    versions: RefCell<HashMap<String, Vec<SemanticVersion>>>,
    /// Manifests resolved while solving, reused by the install phase.
    resolved: RefCell<HashMap<(String, String), Resolved>>,
    error: RefCell<Option<anyhow::Error>>,
}

impl<'a> Provider<'a> {
    pub fn new(
        cfg: &'a Config,
        client: &'a Client,
        root_deps: Vec<(String, Range<SemanticVersion>)>,
    ) -> Self {
        Provider {
            cfg,
            client,
            root_deps,
            versions: RefCell::new(HashMap::new()),
            resolved: RefCell::new(HashMap::new()),
            error: RefCell::new(None),
        }
    }

    /// Semver tags of a package, newest first (cached).
    pub fn versions_of(&self, name: &str) -> Result<Vec<SemanticVersion>> {
        if let Some(v) = self.versions.borrow().get(name) {
            return Ok(v.clone());
        }
        let tags = self.client.list_tags(&self.cfg.repo_for(name))?;
        let mut versions: Vec<SemanticVersion> = tags
            .iter()
            .filter_map(|t| semver::Version::parse(t).ok())
            .filter(|v| v.pre.is_empty() && v.build.is_empty())
            .map(|v| to_pg(&v))
            .collect();
        versions.sort();
        versions.reverse();
        self.versions
            .borrow_mut()
            .insert(name.to_string(), versions.clone());
        Ok(versions)
    }

    fn resolve_version(&self, name: &str, version: &SemanticVersion) -> Result<Resolved> {
        let key = (name.to_string(), version.to_string());
        if let Some(r) = self.resolved.borrow().get(&key) {
            return Ok(r.clone());
        }
        let resolved = self
            .client
            .resolve(&self.cfg.repo_for(name), &version.to_string())?;
        self.resolved.borrow_mut().insert(key, resolved.clone());
        Ok(resolved)
    }

    pub fn take_resolved(&self, name: &str, version: &str) -> Option<Resolved> {
        self.resolved
            .borrow_mut()
            .remove(&(name.to_string(), version.to_string()))
    }
}

impl DependencyProvider<String, SemanticVersion> for Provider<'_> {
    fn choose_package_version<T: Borrow<String>, U: Borrow<Range<SemanticVersion>>>(
        &self,
        potential_packages: impl Iterator<Item = (T, U)>,
    ) -> std::result::Result<(T, Option<SemanticVersion>), Box<dyn std::error::Error>> {
        let list = |p: &String| -> std::vec::IntoIter<SemanticVersion> {
            if p == ROOT {
                return vec![SemanticVersion::new(0, 0, 0)].into_iter();
            }
            match self.versions_of(p) {
                Ok(v) => v.into_iter(),
                Err(e) => {
                    self.error.borrow_mut().get_or_insert(e);
                    Vec::new().into_iter()
                }
            }
        };
        let (pkg, version) = choose_package_with_fewest_versions(list, potential_packages);
        if let Some(e) = self.error.borrow_mut().take() {
            return Err(e.into());
        }
        Ok((pkg, version))
    }

    fn get_dependencies(
        &self,
        package: &String,
        version: &SemanticVersion,
    ) -> std::result::Result<Dependencies<String, SemanticVersion>, Box<dyn std::error::Error>>
    {
        if package == ROOT {
            return Ok(Dependencies::Known(
                self.root_deps.iter().cloned().collect(),
            ));
        }
        let resolved = self.resolve_version(package, version)?;
        let mut deps: Map<String, Range<SemanticVersion>> = Map::default();
        for req in requirements(&resolved) {
            let (name, constraint) = parse_requirement(&req);
            deps.insert(name, parse_range(constraint.as_deref())?);
        }
        Ok(Dependencies::Known(deps))
    }
}

/// Requirements declared on a resolved artifact.
pub fn requirements(resolved: &Resolved) -> Vec<String> {
    resolved
        .annotation(oci::ANNOTATION_REQUIRES)
        .map(|r| {
            r.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Solve `root_deps` into a concrete (name, version) plan.
pub fn solve(provider: &Provider) -> Result<Vec<(String, String)>> {
    match resolve(provider, ROOT.to_string(), SemanticVersion::new(0, 0, 0)) {
        Ok(solution) => Ok(solution
            .into_iter()
            .filter(|(name, _)| name != ROOT)
            .map(|(name, version)| (name, version.to_string()))
            .collect()),
        Err(pubgrub::error::PubGrubError::NoSolution(mut tree)) => {
            tree.collapse_no_versions();
            bail!(
                "version solving failed:\n{}",
                DefaultStringReporter::report(&tree)
            );
        }
        Err(pubgrub::error::PubGrubError::ErrorChoosingPackageVersion(e)) => {
            bail!("version solving failed: {e}")
        }
        Err(pubgrub::error::PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => bail!("version solving failed for {package} {version}: {source}"),
        Err(e) => bail!("version solving failed: {e}"),
    }
}
