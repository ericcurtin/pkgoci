# Introducing pkgoci: a package manager where every package is an OCI artifact

*July 17, 2026 (updated July 18, 2026 with a heavier stress test: Fedora's
`kubernetes` spec, and the two Pkgocifile changes it took to build it)*

Today I'm publishing [pkgoci](https://github.com/ericcurtin/pkgoci), a small
experiment with a big premise: **what if a package manager didn't need any
package infrastructure at all?**

No formula repository. No package index to sync. No custom CDN, no custom
metadata service, no custom protocol, not even for dependency resolution or
signatures. Just OCI registries, the same infrastructure that already serves
billions of container image pulls a day, holding ordinary, spec-compliant
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
  selected via the standard `platform.os`/`platform.architecture` fields:
  the exact mechanism `docker pull` uses to pick `linux/arm64` vs
  `linux/amd64`. Metadata lives in standard `org.opencontainers.image.*`
  annotations. Versions are tags.
- **Because platform selection is standard, platforms are free.** pkgoci
  ships native packages for **macOS aarch64, Linux x86_64/aarch64, and
  Windows x86_64/aarch64**, including the Windows targets Homebrew has never
  supported. All five are built and tested in CI, natively, on every commit.
- **Docker Hub is the default registry**, not ghcr, but it's one environment
  variable to point at any OCI registry, including a `registry:2` container
  on localhost for hacking.

## Real version solving, without a solver service

Packages declare semver-constrained requirements: one line in the
`Pkgocifile`, one more standard annotation on the artifact:

```text
REQUIRES libfoo@^1.2
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

## It builds from source, too

A `Pkgocifile` doesn't have to pack prebuilt trees; it can build them. This
is the real recipe for upstream Lua, from the repo's `examples/`:

```text
NAME lua
VERSION 5.4.8
DESCRIPTION Powerful, efficient, lightweight, embeddable scripting language
LICENSE MIT
FETCH https://www.lua.org/ftp/lua-${PKGOCI_VERSION}.tar.gz 4f18ddae...629ae
SOURCE
RUN:darwin make macosx -j4
RUN:linux make linux -j4
RUN:windows make mingw -j4
RUN make install INSTALL_TOP=$PWD/out
TEST ./out/bin/lua -e 'print("ok")'
OUTPUT ./out
```

`FETCH` pulls the sha256-pinned upstream tarball (the digest also lands in
the build provenance), `RUN` steps execute in a scratch copy of the context
(never mutating it, like a Docker build), and `pkgoci build` packs `OUTPUT`
for the host platform. `TEST` steps (Homebrew's `test do`, in one line each)
gate the build, `ENV` sets build variables, `FROM` picks the Linux build
image exactly like a Dockerfile, and long lines continue with a backslash.
And here is the interesting part: `SOURCE` publishes the post-fetch source
tree *and its build recipe* as one more platform entry in the same artifact
(`source/all`, next to `darwin/arm64` and friends), so `TEST` and `ENV`
travel with the source and gate install-time rebuilds too.

That gives pkgoci the same graceful degradation Homebrew gets from
build-from-source formulas: on a platform with prebuilt binaries, `install`
downloads them; on a platform without, it transparently fetches the source
layer, runs the recipe, and installs the result. `pkgoci install -s` forces a
source build anywhere. And because the source layer and its recipe live under
the same signed digest as everything else, **signature verification happens
before a single build step runs**.

### Builds are sandboxed on every OS

A build recipe is arbitrary code, so `RUN` steps never execute directly on
your machine. Each OS uses its native isolation:

- **Linux**: Docker. The work tree is the only mount, the container runs as
  your uid with `--network=none`, and the build environment is an image you
  pick with `FROM` (default `buildpack-deps:bookworm`), exactly like a
  Dockerfile.
- **macOS**: seatbelt (`sandbox-exec`), the same mechanism Homebrew's build
  sandbox uses, with writes confined to the work tree and temp dirs and the
  network denied.
- **Windows**: a SAFER *constrained* restricted token; the work tree is the
  only location ACLed for the RESTRICTED SID.

This applies to `pkgoci build` and to install-time source builds alike, and
it's enforced, not advisory: a recipe that tries to `touch $HOME/ESCAPED` or
`curl example.com` fails the build (both are asserted in CI). Since `FETCH`
runs before the sandbox and pins digests, recipes have no reason to touch the
network at all.

### Is the format enough for real software?

To find out, I took two packages from each of three ecosystems (**xz** and
**sqlite** from Fedora, **jq** and **lua** from Homebrew, **ninja** and
**zstd** from winget) and packaged all six from their upstream release
tarballs with nothing but a Pkgocifile each. Every one builds, signs,
attests, pushes, installs, and runs, on macOS and in CI on Linux (including
rebuilding Lua from its *published* source layer):

```text
$ pkgoci install xz sqlite jq lua ninja zstd
...
$ jq --version && lua -v && ninja --version
jq-1.7.1
Lua 5.4.8  Copyright (C) 1994-2025 Lua.org, PUC-Rio
1.13.1
```

Getting there required exactly two additions to the format, `FETCH` (pinned
upstream sources) and `RUN:<os>` (Lua's `make macosx`/`make linux`/`make
mingw` build targets), which is the point of testing against real software
instead of hello-world. But six single-binary formulas is the easy end of
the spectrum; the interesting failures show up with something bigger.

### The stress test: Fedora's `kubernetes` spec

So I packaged that too. Fedora 44 builds Kubernetes from one source tree
into **four** RPMs that must always ship in lockstep — `kubernetes`
(kubelet), `kubernetes-client` (kubectl), `kubernetes-kubeadm`, and
`kubernetes-systemd` (kube-apiserver/controller-manager/scheduler/proxy) —
plus a real runtime dependency, `kubernetes-cni`. Pointing a Pkgocifile at
the same upstream source and `go-vendor-tools` vendor tree immediately hit
two gaps:

**One build, several packages.** A Pkgocifile described exactly one
package. Fedora's spec builds four RPMs from one `%build`; there was no way
to say that in the format at all short of fetching and compiling the same
40 MB source tree four separate times. So `NAME` became repeatable: it
starts a new package, and everything that's genuinely per-package
(`DESCRIPTION`, `LICENSE`, `REQUIRES`, `OUTPUT`, `PLATFORM`) scopes to
whichever `NAME` came before it, while the shared build (`VERSION`, `FETCH`,
`RUN`, `TEST`, `ENV`, `FROM`) runs exactly once:

```dockerfile
NAME kubernetes
VERSION 1.36.2
REQUIRES kubernetes-cni@^1.9
OUTPUT ./out-kubelet

NAME kubernetes-client
OUTPUT ./out-client

FETCH https://.../kubernetes-${PKGOCI_VERSION}.tar.gz <sha256>
FETCH https://.../kubernetes-${PKGOCI_VERSION}-vendor.tar.bz2 <sha256>
RUN go build -o out-kubelet/bin/kubelet ./cmd/kubelet
RUN go build -o out-client/bin/kubectl ./cmd/kubectl
```

`pkgoci build` now fetches and compiles once and packs each `NAME` into its
own image, so `pkgoci push kubernetes-client` and `pkgoci push kubernetes`
are independently installable, independently versioned artifacts that were
still built together from one recipe. And because they share one
`VERSION`, a bare `REQUIRES kubernetes` on a sibling defined in the same
file is auto-pinned to that exact version — the same guarantee Fedora's
spec gets from a page of manual `Conflicts: %{name} < %{version}` lines,
for free, just from the file's structure.

**FETCH assumed too much.** The existing `FETCH` only handled `.tar.gz` and
always stripped one leading path component, because that's what a GitHub
release tarball looks like. Fedora's vendor tarball — the pre-run `go mod
vendor` output, published separately because go.mod-based projects don't
commit `vendor/` to git — is `.tar.bz2` and has *no* wrapping directory
(`vendor/`, `go.work`, `go.mod` sit right at the top). `FETCH` now decodes
`.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, `.tar.zst`/`.tzst`, and bare `.tar`,
and only strips a leading component when every entry in the archive
actually shares one — so both shapes just work, with no new syntax to
learn for the common case.

With those two changes, the whole thing builds: kubelet, kubectl, kubeadm,
kube-apiserver, kube-controller-manager, kube-scheduler, and kube-proxy, all
compiled from Fedora's real upstream source and vendor tree, network-denied
throughout (the sandbox only ever touches the network during the digest-pinned
`FETCH`, before the build starts):

```text
$ pkgoci install kubernetes-client kubernetes-kubeadm kubernetes-systemd
...
$ kubectl version --client && kubeadm version
Client Version: v1.36.2
kubeadm version: &version.Info{GitVersion:"v1.36.2", ...}
$ pkgoci info kubernetes-kubeadm
Requires: kubernetes-cni@^1.9, kubernetes@1.36.2
```

(pkgoci has no service manager — no systemd units, no `/etc` — so unlike
Fedora's RPMs this only ever installs the binaries; wiring up kubelet or
kube-apiserver as a system service is left to the host, same as it would be
for anything you `go install`ed by hand. The full recipe, dependency
included, is `examples/kubernetes/Pkgocifile` and
`examples/kubernetes-cni/Pkgocifile` in the repo.)

## Signatures and build provenance, cosign-verifiable

Signing follows the same rule: no new infrastructure, and no new formats.
pkgoci signs the sigstore *simple signing* payload with an ed25519 key and
stores it in the registry, next to the package, using cosign's storage
convention (the `sha256-<digest>.sig` tag and
`dev.cosignproject.cosign/signature` annotation).

And because `pkgoci build` is the thing that builds the package, it records
**SLSA v1 build provenance** while doing so: an in-toto statement pinning the
Pkgocifile digest, the source digest, the exact build steps, the builder, and
timestamps. `push --sign` publishes that provenance as a DSSE attestation
under cosign's `sha256-<digest>.att` tag, the same shape as the build
attestations Homebrew attaches to its bottles.

And signatures don't have to stay private: `--rekor` records them in the
**Rekor transparency log**, the same public, append-only log that backs
sigstore and Homebrew's attestations, and stores the log's receipt (the
Signed Entry Timestamp) alongside the signature, where `pkgoci verify`
checks it against the log's key and confirms it binds this exact signature
and payload.

```sh
pkgoci keygen                       # standard PEM keypair
pkgoci push mytool --sign --rekor   # signature + provenance + tlog entry

# consumers opt in, then verification is enforced:
export PKGOCI_VERIFY_KEY=/path/to/pkgoci.pub
pkgoci install mytool
pkgoci verify mytool                # signature + tlog + provenance report
```

```text
$ pkgoci verify mytool
OK: ...:1.2.3 (sha256:3b5f2b...) verified with pkgoci.pub
OK: transparency log entry 108e91... (index 2189219413) at https://rekor.sigstore.dev verified
OK: build provenance (https://slsa.dev/provenance/v1) by https://pkgoci.dev/pkgoci/0.1.0 ... verified
```

Because the formats are cosign's, the industry-standard tooling agrees with
us on both:

```sh
$ cosign verify --key pkgoci.pub --insecure-ignore-tlog=true .../mytool:1.2.3
  - The signatures were verified against the specified public key
$ cosign verify-attestation --key pkgoci.pub --type slsaprovenance1 \
    --insecure-ignore-tlog=true .../mytool@sha256:...
  - The signatures were verified against the specified public key
```

With `PKGOCI_VERIFY_KEY` set (a key, or a directory of trusted keys),
installs **fail closed**: a missing signature, a signature from an untrusted
key, or a tampered artifact aborts the install, for every dependency in the
plan, not just the package you asked for, and before any source build step
executes. The digest chain does the rest: the signature covers the index, the
index pins the manifests, the manifests pin the blobs, and the provenance
subject pins the index.

For comparison, Homebrew's attestation verification is opt-in, applies to
homebrew-core bottles, and shells out to the external `gh` binary; pkgoci's
verification is built in, works for any publisher on any registry, covers
signatures, provenance, *and* the transparency log receipt, and enforces the
entire dependency graph.

## It's fast: measurably faster than Homebrew on everything

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

The network-bound commands converge toward network latency, as they should,
but the local commands you run dozens of times a day are one to two orders of
magnitude faster.

## Publishing works like Docker: build, then push

There's no formula to write and no PR to send. You describe the package once
in a `Pkgocifile`, deliberately reminiscent of a Dockerfile, and then it's
the two verbs every container user already knows:

```text
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
pkgoci build              # like docker build: Pkgocifile -> local store
pkgoci push mytool --sign # like docker push: local store -> registry
```

`build` runs any `RUN` steps, packs each platform tree into a layer, records
the build provenance, and writes a standard **OCI image layout** into the
local store. Every blob is content-addressed, so the digest `build` prints is
exactly the digest `push` tags, signs, and attests. `push` uploads it to any
registry you have credentials for, as an image index tagged `1.2.3` and
`latest`, plus the cosign-format signature and provenance attestation. Your
users then `PKGOCI_NAMESPACE=yourname pkgoci install mytool`.

Distribution, hosting, bandwidth, auth, mirrors: all outsourced to registry
operators who already solved those problems for containers.

## Honest caveats

This is a young project, and I'd rather list its gaps than oversell it:

- **There's no package catalog yet.** The design removes the need for
  *infrastructure*, not for *packages*. The default `pkgoci` namespace on
  Docker Hub needs to be populated before `pkgoci install jq` means anything.
- **A sandbox is a boundary, not a review.** Builds can't touch your files or
  the network, but you're still running and installing the publisher's code;
  signatures and provenance tell you *who* you're trusting, not that it's
  safe. And a `RUN make` is not a formula DSL: casks, services, and the rest
  of Homebrew's two decades of ecosystem are not what this replaces.
- **Built-in signing is key-based.** Verification enforces keys you chose to
  trust; there's no keyless OIDC-identity flow built in yet. (Because the
  storage formats are cosign's, keyless signing already works *via* cosign
  itself, and the transparency log is the same Rekor either way.)

## Try it

```sh
git clone https://github.com/ericcurtin/pkgoci && cd pkgoci
cargo build --release   # needs Rust + Go toolchains

# Full roundtrip against a local registry:
docker run -d --rm -p 5001:5000 registry:2
export PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=test PKGOCI_PREFIX=/tmp/pkgoci
mkdir -p /tmp/hello/out/bin && printf '#!/bin/sh\necho hi\n' > /tmp/hello/out/bin/hi && chmod +x /tmp/hello/out/bin/hi
printf 'NAME hello\nVERSION 1.0.0\nPLATFORM darwin/arm64 ./out\n' > /tmp/hello/Pkgocifile  # or linux/amd64, ...
./target/release/pkgoci keygen
./target/release/pkgoci build /tmp/hello
./target/release/pkgoci push hello --sign
PKGOCI_VERIFY_KEY=/tmp/pkgoci/keys/pkgoci.pub ./target/release/pkgoci install hello
/tmp/pkgoci/bin/hi
```

The code is Apache-2.0 on
[GitHub](https://github.com/ericcurtin/pkgoci). Issues, benchmark
reproductions, and packages for the ecosystem are all very welcome.

*Registries all the way down.*
