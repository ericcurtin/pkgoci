use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let lib = out.join("libpkgocigo.a");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let goos = match target_os.as_str() {
        "macos" => "darwin",
        other => other,
    };
    let goarch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("unsupported arch: {other}"),
    };

    let mut cmd = Command::new("go");
    cmd.args(["build", "-trimpath", "-buildmode=c-archive", "-o"])
        .arg(&lib)
        .arg(".")
        .current_dir("go")
        .env("CGO_ENABLED", "1")
        .env("GOOS", goos)
        .env("GOARCH", goarch);
    // Respect a cross C compiler if the environment provides one (cgo needs it).
    if let Ok(cc) = env::var("PKGOCI_GO_CC") {
        cmd.env("CC", cc);
    }
    let status = cmd.status().expect(
        "the Go toolchain is required to build pkgoci (containerd is linked in via c-archive)",
    );
    assert!(
        status.success(),
        "go build of the containerd archive failed"
    );

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pkgocigo");
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=Security");
            println!("cargo:rustc-link-lib=resolv");
        }
        "windows" => {
            for l in ["ws2_32", "winmm", "ntdll", "userenv", "bcrypt"] {
                println!("cargo:rustc-link-lib={l}");
            }
        }
        _ => println!("cargo:rustc-link-lib=pthread"),
    }
    println!("cargo:rerun-if-changed=go/main.go");
    println!("cargo:rerun-if-changed=go/go.mod");
    println!("cargo:rerun-if-changed=go/go.sum");
}
