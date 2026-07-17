mod cellar;
mod commands;
mod config;
mod extract;
mod oci;
mod platform;
mod registry;
mod sign;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;

#[derive(Parser)]
#[command(
    name = "pkgoci",
    version,
    about = "Fast, native package manager backed by OCI registries (Docker Hub by default)",
    after_help = "Environment:\n  PKGOCI_PREFIX     install prefix (default: ~/.pkgoci)\n  PKGOCI_REGISTRY   OCI registry (default: registry-1.docker.io)\n  PKGOCI_NAMESPACE  default repository namespace (default: pkgoci)\n  PKGOCI_USERNAME / PKGOCI_PASSWORD  registry credentials (push)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install packages (name, name@version, or org/name)
    Install {
        packages: Vec<String>,
        /// Reinstall even if already installed
        #[arg(short, long)]
        force: bool,
    },
    /// Uninstall packages
    #[command(alias = "remove", alias = "rm")]
    Uninstall {
        packages: Vec<String>,
        /// Remove even if other installed packages require it
        #[arg(short, long)]
        force: bool,
    },
    /// List installed packages
    #[command(alias = "ls")]
    List,
    /// Show package details
    Info { package: String },
    /// Search packages in the configured namespace
    Search { term: String },
    /// Upgrade installed packages (all, or the given ones)
    Upgrade { packages: Vec<String> },
    /// Refresh package metadata (no-op: metadata is resolved live)
    Update,
    /// Remove the download cache and outdated kegs
    Cleanup,
    /// Print the install prefix
    Prefix,
    /// Publish a directory tree as a package (requires credentials)
    Push {
        name: String,
        /// Package version, e.g. 1.2.3
        #[arg(long)]
        version: String,
        /// Platform payloads: os/arch=path (repeatable), e.g. darwin/arm64=./out
        #[arg(long = "dir")]
        dirs: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        license: Option<String>,
        /// Runtime dependency (repeatable), e.g. --requires libfoo
        #[arg(long = "requires")]
        requires: Vec<String>,
        /// Sign the package with the key from `pkgoci keygen`
        #[arg(long)]
        sign: bool,
    },
    /// Generate an ed25519 signing keypair
    Keygen {
        /// Output directory (default: <prefix>/keys)
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();
    match cli.cmd {
        Cmd::Install { packages, force } => commands::install(&cfg, packages, force),
        Cmd::Uninstall { packages, force } => commands::uninstall(&cfg, packages, force),
        Cmd::List => commands::list(&cfg),
        Cmd::Info { package } => commands::info(&cfg, package),
        Cmd::Search { term } => commands::search(&cfg, term),
        Cmd::Upgrade { packages } => commands::upgrade(&cfg, packages),
        Cmd::Update => commands::update(),
        Cmd::Cleanup => commands::cleanup(&cfg),
        Cmd::Prefix => {
            println!("{}", cfg.prefix.display());
            Ok(())
        }
        Cmd::Push {
            name,
            version,
            dirs,
            description,
            license,
            requires,
            sign,
        } => commands::push(
            &cfg,
            name,
            version,
            dirs,
            description,
            license,
            requires,
            sign,
        ),
        Cmd::Keygen { out } => commands::keygen(&cfg, out),
    }
}
