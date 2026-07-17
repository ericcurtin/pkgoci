# pkgoci

A fast, native package manager backed by OCI registries — like Homebrew, but
packages live on **Docker Hub** by default (any OCI registry works).

> New here? Read the introduction:
> [Introducing pkgoci: a package manager where every package is an OCI artifact](https://ericcurtin.github.io/pkgoci/blog/2026-07-17-introducing-pkgoci.html)

The CLI is written in Rust; all registry protocol handling is
**containerd's distribution stack** (`core/remotes/docker` — resolver,
token auth, fetcher, pusher, platform matching), linked into the binary as a
Go c-archive rather than reimplemented. There is no daemon: the containerd
code runs in-process.

Unlike Homebrew, pkgoci supports native packages on **five** platforms:

| OS      | Architectures      |
|---------|--------------------|
| macOS   | aarch64            |
| Linux   | x86_64, aarch64    |
| Windows | x86_64, aarch64    |

## How it works

Packages are ordinary OCI artifacts: an image index with one manifest per
platform (standard `platform.os`/`platform.architecture` fields, so any OCI
tooling understands them), each with a single `tar+gzip` (or `tar+zstd`) layer
containing the package tree (`bin/`, `lib/`, ...). Metadata uses standard
`org.opencontainers.image.*` annotations. Versions are tags.

There is no formula index to sync: metadata is resolved live from the
registry, so `pkgoci update` is a no-op and `install` is one resolve, one
digest-verified blob download (all via containerd), extracted straight into
the Cellar and symlinked (hardlinked/copied on Windows) into `<prefix>/bin`.

## Usage

```sh
pkgoci install jq ripgrep@14.1.1     # parallel, digest-verified
pkgoci uninstall jq
pkgoci list
pkgoci info jq
pkgoci search json
pkgoci upgrade                       # or: pkgoci upgrade jq
pkgoci cleanup                       # drop cache + outdated kegs
pkgoci prefix
```

Add `$(pkgoci prefix)/bin` to your `PATH`.

### Dependencies

Packages declare runtime dependencies with semver constraints (a
`dev.pkgoci.requires` annotation): `--requires 'libfoo@^1.2'`,
`'libfoo@>=1,<3'`, `'libfoo@~1.2.3'`, exact `'libfoo@1.2.3'`, or bare
`libfoo` for any version. `pkgoci install` solves the whole graph with the
**PubGrub** algorithm against the versions available in the registry, picks
the newest satisfying set, and installs it in parallel. Unsatisfiable
constraints fail with a PubGrub derivation, e.g.:

```
Because tool depends on libfoo 1.0.0 <= v < 2.0.0 and app 1.0.0 depends on
libfoo 2.0.0 <= v < 3.0.0, app 1.0.0, tool ∗ are incompatible.
```

CLI specs accept the same constraints (`pkgoci install 'libfoo@>=1,<1.1'`),
and `pkgoci uninstall` refuses to remove a package that another installed
package requires (override with `--force`).

### Signatures

```sh
pkgoci keygen                          # ed25519 keypair (PEM) in <prefix>/keys
pkgoci push mytool ... --sign          # cosign-compatible signature
export PKGOCI_VERIFY_KEY=/path/pkgoci.pub   # a .pub file, or a dir of them
pkgoci install mytool                  # fails closed without a valid signature
pkgoci verify mytool                   # explicit check
```

Signatures use the sigstore simple-signing payload and cosign's storage
convention (`sha256-<digest>.sig` tag, `dev.cosignproject.cosign/signature`
annotation), so they also verify with stock cosign:

```sh
cosign verify --key pkgoci.pub --insecure-ignore-tlog=true registry-1.docker.io/pkgoci/mytool:1.2.3
```

When `PKGOCI_VERIFY_KEY` is set (one key or a directory of trusted keys),
every install — dependencies included — must carry a signature that verifies
against a trusted key; missing, mismatched, and tampered signatures all abort
the install. Verification is built in: no external tooling is needed.

### Publishing

Publishing (needs `PKGOCI_USERNAME`/`PKGOCI_PASSWORD`):

```sh
pkgoci push mytool --version 1.2.3 --license MIT \
  --description "My tool" \
  --requires libfoo --sign \
  --dir darwin/arm64=./out/mac-arm64 \
  --dir linux/amd64=./out/linux-amd64 \
  --dir linux/arm64=./out/linux-arm64 \
  --dir windows/amd64=./out/win-amd64 \
  --dir windows/arm64=./out/win-arm64
```

### Configuration

| Variable             | Default                 |
|----------------------|-------------------------|
| `PKGOCI_PREFIX`      | `~/.pkgoci` (`%LOCALAPPDATA%\pkgoci` on Windows) |
| `PKGOCI_REGISTRY`    | `registry-1.docker.io`  |
| `PKGOCI_NAMESPACE`   | `pkgoci`                |
| `PKGOCI_SIGNING_KEY` | `<prefix>/keys/pkgoci.key` |
| `PKGOCI_VERIFY_KEY`  | unset (no verification) |

Names may include a namespace (`pkgoci install someorg/sometool`), and
`localhost`/`127.*` registries are reached over plain HTTP (containerd's
`MatchLocalhost`) for local testing.

## Benchmarks vs Homebrew

`bench/bench.sh` (hyperfine, macOS aarch64, Homebrew 6.0.6):

| Benchmark            | pkgoci   | brew     | Speedup |
|----------------------|----------|----------|---------|
| startup (`--version`)| 11.5 ms  | 278.5 ms | **24x** |
| `prefix`             | 7.3 ms   | 40.1 ms  | **5.5x**|
| `list` (installed)   | 10.5 ms  | 1.130 s  | **108x**|
| `update`             | 7.1 ms   | 2.807 s  | **398x**|
| `info` (network)     | 1.048 s  | 1.227 s  | **1.2x**|
| `search` (network)   | 385 ms   | 1.134 s  | **2.9x**|

## Building & testing

Requires Rust and Go toolchains (Go builds the containerd c-archive via
`build.rs`; on Windows use the `-gnu`/`-gnullvm` Rust targets with mingw,
since Go archives use the GNU ABI).

```sh
cargo build --release

# End-to-end roundtrip against a local registry:
docker run -d --rm --name reg -p 5001:5000 registry:2
export PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=test PKGOCI_PREFIX=/tmp/pkgoci
mkdir -p /tmp/hello/bin && printf '#!/bin/sh\necho hi\n' > /tmp/hello/bin/hi && chmod +x /tmp/hello/bin/hi
./target/release/pkgoci push hello --version 1.0.0 --dir darwin/arm64=/tmp/hello
./target/release/pkgoci install hello && /tmp/pkgoci/bin/hi
```
