mod cellar;
mod commands;
mod config;
mod extract;
mod oci;
mod pkgocifile;
mod platform;
mod registry;
mod rekor;
mod resolve;
mod sandbox;
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
        /// Build from the published source instead of prebuilt binaries
        #[arg(short = 's', long)]
        build_from_source: bool,
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
    /// Build a package from a Pkgocifile into the local store
    Build {
        /// Directory containing the Pkgocifile
        #[arg(default_value = ".")]
        path: std::path::PathBuf,
        /// Alternate Pkgocifile path
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
    },
    /// Push a built package to the registry (requires credentials)
    Push {
        /// Built package (name or name@version) from `pkgoci build`
        package: String,
        /// Sign the package with the key from `pkgoci keygen`
        #[arg(long)]
        sign: bool,
        /// Record the signature in the Rekor transparency log
        #[arg(long)]
        rekor: bool,
    },
    /// Generate an ed25519 signing keypair
    Keygen {
        /// Output directory (default: <prefix>/keys)
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Verify a package's signature
    Verify {
        package: String,
        /// Public key or directory of .pub keys (default: PKGOCI_VERIFY_KEY)
        #[arg(long)]
        key: Option<std::path::PathBuf>,
    },
}

fn main() {
    // Die quietly on closed pipes (`pkgoci ... | head`) instead of
    // panicking: restore the default SIGPIPE disposition Rust masks.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // Internal Windows sandbox helper: `pkgoci __sandbox-exec <dir> <cmd>`.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() == 4 && args[1] == "__sandbox-exec" {
            match sandbox::windows_exec_restricted(std::path::Path::new(&args[2]), &args[3]) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("error: {e:#}");
                    std::process::exit(1);
                }
            }
        }
    }
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();
    match cli.cmd {
        Cmd::Install {
            packages,
            force,
            build_from_source,
        } => commands::install(&cfg, packages, force, build_from_source),
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
        Cmd::Build { path, file } => commands::build(&cfg, path, file),
        Cmd::Push {
            package,
            sign,
            rekor,
        } => commands::push(&cfg, package, sign, rekor),
        Cmd::Keygen { out } => commands::keygen(&cfg, out),
        Cmd::Verify { package, key } => commands::verify(&cfg, package, key),
    }
}
