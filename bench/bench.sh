#!/usr/bin/env bash
# Benchmark pkgoci against Homebrew with hyperfine.
#
# Requirements: hyperfine, brew, a release build of pkgoci, and (for the
# install benchmark) a local OCI registry with a test package:
#
#   docker run -d --rm --name pkgoci-bench-reg -p 5001:5000 registry:2
#   PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=bench \
#     pkgoci push hello --version 1.0.0 --dir "$(uname | tr A-Z a-z | sed s/darwin/darwin/)/arm64=<dir>"
set -euo pipefail

PKGOCI=${PKGOCI:-$(dirname "$0")/../target/release/pkgoci}
RUNS=${RUNS:-10}

echo "pkgoci: $($PKGOCI --version)"
echo "brew:   $(brew --version | head -1)"
echo

run() {
  local name=$1 a=$2 b=$3
  echo "### $name"
  hyperfine --warmup 2 --runs "$RUNS" --ignore-failure -n "pkgoci" "$a" -n "brew" "$b"
}

run "startup (--version)" "$PKGOCI --version" "brew --version"
run "prefix"              "$PKGOCI prefix"    "brew --prefix"
run "list installed"      "$PKGOCI list"      "brew list --versions"
run "update"              "$PKGOCI update"    "brew update"
run "info (network)"      "$PKGOCI info library/alpine" "brew info wget"
run "search (network)"    "PKGOCI_NAMESPACE=library $PKGOCI search alpine" "brew search wget"

# Install/uninstall loop against a local registry (optional; set BENCH_INSTALL=1).
if [[ "${BENCH_INSTALL:-0}" == "1" ]]; then
  export PKGOCI_REGISTRY=localhost:5001 PKGOCI_NAMESPACE=bench
  run "install + uninstall" \
    "$PKGOCI install hello --force && $PKGOCI uninstall hello" \
    "brew install hello 2>/dev/null; brew uninstall hello"
fi
