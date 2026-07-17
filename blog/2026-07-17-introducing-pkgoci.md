# Introducing pkgoci: a package manager where every package is an OCI artifact

*July 17, 2026*

Today I'm publishing [pkgoci](https://github.com/ericcurtin/pkgoci), a small
experiment with a big premise: **what if a package manager didn't need any
package infrastructure at all?**

No formula repository. No package index to sync. No custom CDN, no custom
metadata service, no custom protocol. Just OCI registries — the same
infrastructure that already serves billions of container image pulls a day —
holding ordinary, spec-compliant artifacts that any OCI tool can inspect.

```sh
pkgoci install hello
```

That one command does a manifest resolve and a digest-verified blob download
from Docker Hub, extracts the payload into a Homebrew-style Cellar, and links
the binaries into your `PATH`. That's the whole trick.

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

## Don't reimplement the registry — embed containerd

Registry protocol code is deceptively fiddly: token challenges, anonymous
auth, manifest lists vs indexes, digest verification, resumable pushes,
platform variant matching. Rather than write another implementation of all
that, pkgoci links in the code that already handles more registry traffic
than anything else on earth: **containerd's distribution stack**.

The Go packages containerd itself uses to talk to registries
(`core/remotes/docker`: resolver, authorizer, fetcher, pusher, plus its
platform matcher) are compiled with `go build -buildmode=c-archive` and
statically linked into the Rust binary. Four C functions cross the FFI
boundary — resolve, fetch blob, push blob, push manifest — with JSON across
the seam. There is **no daemon**: containerd's client code runs in-process,
and the result is still a single self-contained executable.

The Rust side keeps everything that makes it a package manager rather than a
container tool: the CLI, the Cellar layout and receipts, linking/shims,
tar+gzip/zstd extraction, upgrades, cleanup.

## It's fast — measurably faster than Homebrew on everything

A package manager is a CLI you run interactively, so latency is the product.
Two design decisions do most of the work here:

1. **A native binary.** No interpreter to boot. Homebrew spends hundreds of
   milliseconds starting Ruby before it reads your command.
2. **No index to maintain.** Package metadata is resolved live from the
   registry, so `pkgoci update` has literally nothing to do, and there's no
   multi-second `brew update` tax to pay.

`hyperfine` numbers from an M-series MacBook (Homebrew 6.0.6, `bench/bench.sh`
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
a platform, you can publish it with one command:

```sh
pkgoci push mytool --version 1.2.3 --license MIT \
  --description "My tool" \
  --dir darwin/arm64=./out/mac-arm64 \
  --dir linux/amd64=./out/linux-amd64 \
  --dir linux/arm64=./out/linux-arm64 \
  --dir windows/amd64=./out/win-amd64 \
  --dir windows/arm64=./out/win-arm64
```

Each directory becomes a `tar+gzip` layer, each platform a manifest, the set
an image index tagged `1.2.3` and `latest` — pushed through containerd's
pusher to any registry you have credentials for. Your users then
`PKGOCI_NAMESPACE=yourname pkgoci install mytool`.

Distribution, hosting, bandwidth, auth, mirrors: all outsourced to registry
operators who already solved those problems for containers.

## Honest caveats

This is a young project, and I'd rather list its gaps than oversell it:

- **There's no package catalog yet.** The design removes the need for
  *infrastructure*, not for *packages*. The default `pkgoci` namespace on
  Docker Hub needs to be populated before `pkgoci install jq` means anything.
- **No dependency resolution.** Today a package is a self-contained tree.
  That's fine for static binaries (a large and growing share of modern CLI
  tools) and wrong for C libraries with deep dependency graphs. Homebrew's
  formula DSL, build-from-source support, casks, services, and taps represent
  two decades of ecosystem pkgoci does not replace.
- **No signatures yet.** Downloads are digest-verified end to end, but there
  is no sigstore/cosign-style signing story yet — although storing packages
  as OCI artifacts means those tools slot in naturally.
- **The embedded Go runtime costs something**: the binary is ~11 MB instead
  of ~2.5 MB, and startup is ~11 ms instead of ~4 ms. That trade bought us
  containerd's battle-tested registry code instead of a hand-rolled client.

## Try it

```sh
git clone https://github.com/ericcurtin/pkgoci && cd pkgoci
cargo build --release   # needs Rust + Go toolchains

# Full roundtrip against a local registry:
docker run -d --rm -p 5001:5000 registry:2
export PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=test PKGOCI_PREFIX=/tmp/pkgoci
mkdir -p /tmp/hello/bin && printf '#!/bin/sh\necho hi\n' > /tmp/hello/bin/hi && chmod +x /tmp/hello/bin/hi
./target/release/pkgoci push hello --version 1.0.0 --dir darwin/arm64=/tmp/hello  # or linux/amd64, ...
./target/release/pkgoci install hello
/tmp/pkgoci/bin/hi
```

The code is Apache-2.0 on
[GitHub](https://github.com/ericcurtin/pkgoci). Issues, benchmarks
reproductions, and packages for the ecosystem are all very welcome.

*Registries all the way down.*
