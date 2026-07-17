# Introducing pkgoci: a package manager where every package is an OCI artifact

*July 17, 2026*

Today I'm publishing [pkgoci](https://github.com/ericcurtin/pkgoci), a small
experiment with a big premise: **what if a package manager didn't need any
package infrastructure at all?**

No formula repository. No package index to sync. No custom CDN, no custom
metadata service, no custom protocol — not even for dependency resolution or
signatures. Just OCI registries — the same infrastructure that already serves
billions of container image pulls a day — holding ordinary, spec-compliant
artifacts that any OCI tool can inspect.

```sh
pkgoci install hello
```

That one command resolves the package, solves its dependency constraints,
verifies signatures, downloads digest-verified blobs from Docker Hub,
extracts everything into a Homebrew-style Cellar, and links the binaries into
your `PATH`. That's the whole trick.

## Homebrew got there first (almost)

This idea is half-proven already. Homebrew quietly became one of the largest
OCI users in the world when it moved its binary "bottles" to GitHub Packages:
every `brew install wget` today downloads a tarball stored in an OCI registry
at `ghcr.io`.

But Homebrew treats the registry as a dumb blob store bolted onto a
Ruby-based formula ecosystem, and its artifacts are only half-standard: bottle
selection ignores the OCI `platform` fields and instead matches Homebrew's own
annotations like `sh.brew.bottle.digest` and a `ref.name` of
`1.2.3.arm64_sonoma`. No generic OCI tooling can meaningfully consume a
bottle, and the design is tied to Homebrew's supported platforms: macOS and
Linux.

pkgoci goes the rest of the way:

- **Artifacts are plain OCI.** An image index with one manifest per platform,
  selected via the standard `platform.os`/`platform.architecture` fields —
  the exact mechanism `docker pull` uses to pick `linux/arm64` vs
  `linux/amd64`. Metadata lives in standard `org.opencontainers.image.*`
  annotations. Versions are tags.
- **Because platform selection is standard, platforms are free.** pkgoci
  ships native packages for **macOS aarch64, Linux x86_64/aarch64, and
  Windows x86_64/aarch64** — including the Windows targets Homebrew has never
  supported. All five are built and tested in CI, natively, on every commit.
- **Docker Hub is the default registry**, not ghcr — but it's one environment
  variable to point at any OCI registry, including a `registry:2` container
  on localhost for hacking.

## Real version solving, without a solver service

Packages declare semver-constrained requirements — one more standard
annotation on the artifact:

```sh
pkgoci push tool --version 3.0.0 --requires 'libfoo@^1.2' --dir ...
```

At install time pkgoci reads the available versions straight from the
registry's tags and solves the whole graph with **PubGrub**, the version
solving algorithm behind modern package managers. Constraints compose across
the graph (`^1.2`, `~1.2.3`, `>=1,<3`, exact pins, CLI ranges like
`pkgoci install 'libfoo@>=1,<1.1'`), the newest satisfying set wins, and the
plan installs in parallel. When constraints can't be satisfied, you get
PubGrub's derivation instead of a mystery:

```
Because tool depends on libfoo 1.0.0 <= v < 2.0.0 and app 1.0.0 depends on
libfoo 2.0.0 <= v < 3.0.0, app 1.0.0, tool ∗ are incompatible.
```

Receipts remember the edges, so `pkgoci uninstall libfoo` refuses to strand a
package that still needs it. No lockfile server, no dependency database: the
graph lives on the artifacts themselves.

## Signatures cosign can verify

Signing follows the same rule — no new infrastructure, and no new formats.
pkgoci signs the sigstore *simple signing* payload with an ed25519 key and
stores it in the registry, next to the package, using cosign's storage
convention (the `sha256-<digest>.sig` tag and
`dev.cosignproject.cosign/signature` annotation):

```sh
pkgoci keygen                       # standard PEM keypair
pkgoci push mytool ... --sign

# consumers opt in, then verification is enforced:
export PKGOCI_VERIFY_KEY=/path/to/pkgoci.pub
pkgoci install mytool
pkgoci verify mytool                # explicit check
```

Because the format is cosign's, the industry-standard tooling agrees with us:

```sh
$ cosign verify --key pkgoci.pub --insecure-ignore-tlog=true .../mytool:1.2.3
The following checks were performed on each of these signatures:
  - The cosign claims were validated
  - The signatures were verified against the specified public key
```

With `PKGOCI_VERIFY_KEY` set (a key, or a directory of trusted keys),
installs **fail closed**: a missing signature, a signature from an untrusted
key, or a tampered artifact aborts the install — for every dependency in the
plan, not just the package you asked for. The digest chain does the rest: the
signature covers the index, the index pins the manifests, the manifests pin
the blobs.

For comparison, Homebrew's attestation verification is opt-in, applies to
homebrew-core bottles, and shells out to the external `gh` binary; pkgoci's
verification is built in, works for any publisher on any registry, and
enforces the entire dependency graph.

## It's fast — measurably faster than Homebrew on everything

A package manager is a CLI you run interactively, so latency is the product.
Two design decisions do most of the work here:

1. **A native binary.** No interpreter to boot. Homebrew spends hundreds of
   milliseconds starting Ruby before it reads your command.
2. **No index to maintain.** Package metadata is resolved live from the
   registry, so `pkgoci update` has literally nothing to do, and there's no
   multi-second `brew update` tax to pay.

`hyperfine` numbers from an M-series MacBook (Homebrew 6.0.6,
[`bench/bench.sh`](https://github.com/ericcurtin/pkgoci/blob/main/bench/bench.sh)
in the repo):

| Benchmark             | pkgoci   | brew     | Speedup |
|-----------------------|----------|----------|---------|
| startup (`--version`) | 11.5 ms  | 278.5 ms | **24x** |
| `prefix`              | 7.3 ms   | 40.1 ms  | **5.5x**|
| `list` (installed)    | 10.5 ms  | 1.130 s  | **108x**|
| `update`              | 7.1 ms   | 2.807 s  | **398x**|
| `info` (network)      | 1.048 s  | 1.227 s  | **1.2x**|
| `search` (network)    | 385 ms   | 1.134 s  | **2.9x**|

The network-bound commands converge toward network latency, as they should —
but the local commands you run dozens of times a day are one to two orders of
magnitude faster.

## Publishing is a first-class verb

There's no formula to write and no PR to send. If you can build your tool for
a platform, you can publish it — dependencies, signature, and all — with one
command:

```sh
pkgoci push mytool --version 1.2.3 --license MIT \
  --description "My tool" \
  --requires 'libfoo@^1.2' --sign \
  --dir darwin/arm64=./out/mac-arm64 \
  --dir linux/amd64=./out/linux-amd64 \
  --dir linux/arm64=./out/linux-arm64 \
  --dir windows/amd64=./out/win-amd64 \
  --dir windows/arm64=./out/win-arm64
```

Each directory becomes a `tar+gzip` layer, each platform a manifest, the set
an image index tagged `1.2.3` and `latest`, plus a cosign-format signature —
pushed to any registry you have credentials for. Your users then
`PKGOCI_NAMESPACE=yourname pkgoci install mytool`.

Distribution, hosting, bandwidth, auth, mirrors: all outsourced to registry
operators who already solved those problems for containers.

## Honest caveats

This is a young project, and I'd rather list its gaps than oversell it:

- **There's no package catalog yet.** The design removes the need for
  *infrastructure*, not for *packages*. The default `pkgoci` namespace on
  Docker Hub needs to be populated before `pkgoci install jq` means anything.
- **It distributes binaries; it doesn't build them.** There is no
  build-from-source path, no formula DSL, no casks or services — Homebrew's
  two decades of ecosystem are not what this replaces.
- **Signing is key-based.** There's no keyless flow or transparency log yet;
  key distribution is up to you. Because the format is already cosign's, that
  upgrade path slots in naturally later.

## Try it

```sh
git clone https://github.com/ericcurtin/pkgoci && cd pkgoci
cargo build --release   # needs Rust + Go toolchains

# Full roundtrip against a local registry:
docker run -d --rm -p 5001:5000 registry:2
export PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=test PKGOCI_PREFIX=/tmp/pkgoci
mkdir -p /tmp/hello/bin && printf '#!/bin/sh\necho hi\n' > /tmp/hello/bin/hi && chmod +x /tmp/hello/bin/hi
./target/release/pkgoci keygen
./target/release/pkgoci push hello --version 1.0.0 --sign --dir darwin/arm64=/tmp/hello  # or linux/amd64, ...
PKGOCI_VERIFY_KEY=/tmp/pkgoci/keys/pkgoci.pub ./target/release/pkgoci install hello
/tmp/pkgoci/bin/hi
```

The code is Apache-2.0 on
[GitHub](https://github.com/ericcurtin/pkgoci). Issues, benchmark
reproductions, and packages for the ecosystem are all very welcome.

*Registries all the way down.*
