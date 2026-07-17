use std::path::PathBuf;

pub const DEFAULT_REGISTRY: &str = "registry-1.docker.io";
pub const DEFAULT_NAMESPACE: &str = "pkgoci";

pub struct Config {
    /// Root prefix, e.g. ~/.pkgoci
    pub prefix: PathBuf,
    /// OCI registry host, defaults to Docker Hub.
    pub registry: String,
    /// Default repository namespace on the registry.
    pub namespace: String,
}

impl Config {
    pub fn load() -> Self {
        let prefix = std::env::var_os("PKGOCI_PREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(default_prefix);
        let registry = std::env::var("PKGOCI_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into());
        let namespace =
            std::env::var("PKGOCI_NAMESPACE").unwrap_or_else(|_| DEFAULT_NAMESPACE.into());
        Config {
            prefix,
            registry,
            namespace,
        }
    }

    pub fn cellar(&self) -> PathBuf {
        self.prefix.join("Cellar")
    }

    pub fn bin(&self) -> PathBuf {
        self.prefix.join("bin")
    }

    pub fn cache(&self) -> PathBuf {
        self.prefix.join("cache")
    }

    /// Local build store (`pkgoci build` output, OCI image layouts).
    pub fn store(&self) -> PathBuf {
        self.prefix.join("store")
    }

    /// Private key used by `push --sign`.
    pub fn signing_key(&self) -> PathBuf {
        std::env::var_os("PKGOCI_SIGNING_KEY")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.prefix.join("keys").join("pkgoci.key"))
    }

    /// Public key installs must verify against, if configured.
    pub fn verify_key(&self) -> Option<PathBuf> {
        std::env::var_os("PKGOCI_VERIFY_KEY").map(PathBuf::from)
    }

    pub fn is_docker_hub(&self) -> bool {
        self.registry.ends_with("docker.io")
    }

    /// Full repository path for a package name. Names may already contain a
    /// namespace (`org/name`), otherwise the default namespace is used.
    pub fn repo_for(&self, name: &str) -> String {
        if name.contains('/') {
            name.to_string()
        } else {
            format!("{}/{}", self.namespace, name)
        }
    }
}

fn default_prefix() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("pkgoci");
        }
    }
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".pkgoci")
}
