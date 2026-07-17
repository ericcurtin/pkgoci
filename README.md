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

Packages declare runtime dependencies with semver constraints (`REQUIRES` in
the `Pkgocifile`, a `dev.pkgoci.requires` annotation on the artifact):
`libfoo@^1.2`, `libfoo@>=1,<3`, `libfoo@~1.2.3`, exact `libfoo@1.2.3`, or
bare `libfoo` for any version. `pkgoci install` solves the whole graph with the
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

### Building from source

A `Pkgocifile` can build instead of (or as well as) packing prebuilt trees.
This is `examples/lua/Pkgocifile`, which packages upstream Lua:

```dockerfile
NAME lua
VERSION 5.4.8
DESCRIPTION Powerful, efficient, lightweight, embeddable scripting language
LICENSE MIT
URL https://www.lua.org
FETCH https://www.lua.org/ftp/lua-${PKGOCI_VERSION}.tar.gz 4f18ddae...629ae
SOURCE .
RUN:darwin make macosx -j4
RUN:linux make linux -j4
RUN:windows make mingw -j4
RUN make install INSTALL_TOP=$PWD/out
OUTPUT ./out
```

`FETCH` downloads a sha256-pinned upstream tarball (cached by digest) and
extracts it into the build context; `RUN` steps execute on the host
(`RUN:<os>` limits a step to one OS; `$PKGOCI_OS`/`$PKGOCI_ARCH` are set);
`OUTPUT` is packed for the host platform. The context itself is never
modified — fetching and building happen in a scratch copy, like a Docker
build context.

With `SOURCE`, the post-fetch source tree and its build recipe are published
as part of the package, so `pkgoci install` transparently builds from source
on platforms without prebuilt binaries — and
`pkgoci install -s/--build-from-source` forces it. Signature verification
happens before any build step runs, and `FETCH` digests are recorded in the
build provenance.

### Build sandbox

`RUN` steps never execute directly on your machine:

| OS      | Backend |
|---------|---------|
| Linux   | Docker (`--network=none`, work tree mounted, host uid/gid; image from `IMAGE`, default `buildpack-deps:bookworm`) |
| macOS   | seatbelt (`sandbox-exec`, as Homebrew uses): writes confined to the work tree and temp dirs, network denied |
| Windows | SAFER "constrained" restricted token; the work tree is ACLed for the RESTRICTED SID |

This applies both to `pkgoci build` and to install-time source builds.
`PKGOCI_SANDBOX=0` disables it (e.g. Linux hosts without Docker).

`examples/` packages real software from three ecosystems with this format:
xz and sqlite (Fedora), jq and lua (Homebrew), ninja and zstd (winget) — all
built, installed, and run in CI.

### Signatures

```sh
pkgoci keygen                          # ed25519 keypair (PEM) in <prefix>/keys
pkgoci push mytool --sign              # cosign-compatible signature
export PKGOCI_VERIFY_KEY=/path/pkgoci.pub   # a .pub file, or a dir of them
pkgoci install mytool                  # fails closed without a valid signature
pkgoci verify mytool                   # explicit check
```

Signatures use the sigstore simple-signing payload and cosign's storage
convention (`sha256-<digest>.sig` tag, `dev.cosignproject.cosign/signature`
annotation). `pkgoci build` additionally records **SLSA v1 build provenance**
(an in-toto statement covering the Pkgocifile, source digest, build steps,
builder, and timestamps), which `push --sign` publishes as a DSSE attestation
under cosign's `sha256-<digest>.att` tag. Both verify with stock cosign:

```sh
cosign verify --key pkgoci.pub --insecure-ignore-tlog=true registry-1.docker.io/pkgoci/mytool:1.2.3
cosign verify-attestation --key pkgoci.pub --type slsaprovenance1 \
  --insecure-ignore-tlog=true registry-1.docker.io/pkgoci/mytool@sha256:...
```

`push --sign --rekor` additionally records the signature in the **Rekor
transparency log** (`rekor.sigstore.dev`, or `PKGOCI_REKOR_URL`) and stores
the receipt with the signature; `pkgoci verify` checks the receipt's Signed
Entry Timestamp against the log's key and confirms it binds the exact
signature and payload.

When `PKGOCI_VERIFY_KEY` is set (one key or a directory of trusted keys),
every install — dependencies included — must carry a signature that verifies
against a trusted key; missing, mismatched, and tampered signatures all abort
the install. `pkgoci verify` additionally checks and prints the transparency
log receipt and the build provenance. Verification is built in: no external
tooling is needed.

### Publishing

Publishing is a two-step, Docker-style flow: describe the package once in a
`Pkgocifile`, then `build` and `push` (push needs
`PKGOCI_USERNAME`/`PKGOCI_PASSWORD`):

```dockerfile
# Pkgocifile
NAME mytool
VERSION 1.2.3
DESCRIPTION My tool
LICENSE MIT
REQUIRES libfoo@^1.2
PLATFORM darwin/arm64 ./out/mac-arm64
PLATFORM linux/amd64 ./out/linux-amd64
PLATFORM linux/arm64 ./out/linux-arm64
PLATFORM windows/amd64 ./out/win-amd64
PLATFORM windows/arm64 ./out/win-arm64
```

```sh
pkgoci build             # packs an OCI image layout into <prefix>/store
pkgoci push mytool --sign
```

`build` takes a directory containing a `Pkgocifile` (default `.`, or
`-f/--file`); `push` takes `name` (newest built version) or `name@version`.

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
mkdir -p /tmp/hello/out/bin && printf '#!/bin/sh\necho hi\n' > /tmp/hello/out/bin/hi && chmod +x /tmp/hello/out/bin/hi
printf 'NAME hello\nVERSION 1.0.0\nPLATFORM darwin/arm64 ./out\n' > /tmp/hello/Pkgocifile  # or linux/amd64, ...
./target/release/pkgoci build /tmp/hello
./target/release/pkgoci push hello
./target/release/pkgoci install hello && /tmp/pkgoci/bin/hi
```
