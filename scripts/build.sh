#!/usr/bin/env bash
# Build the zen-man executable, from a bare checkout to a runnable binary in one command.
#
# What it does, in order:
#
#   1. preflight  — the checks whose failure is not a compile error and would otherwise show up as a
#                     confusing rustc/linker message half-way through a long build;
#   2. fetch      — `cargo fetch --locked`, so the rev-pinned forks of Cargo.lock (§5.1) are what get
#                     built and the network part fails early with a network-shaped message;
#   3. build      — `cargo build --release -p zenman`;
#   4. report     — the path, size and hash of the artefact.
#
# `--check` stops after (1), `--debug` swaps (3) for the dev profile, `--smoke` adds the sanctioned
# headless boot test, `--install` and `--dist` add the two things you do with the result.
#
# Why no `cargo install`: this is a workspace of four members whose binary depends on its siblings,
# `cargo install --path` would build it in a temporary target dir against a *fresh* dependency graph,
# and the `[patch.crates-io]` table of the workspace root is not what we want to re-resolve per
# install. Building in-tree keeps the lockfile authoritative.
#
# Windows: `cargo build --release -p zenman` is the whole story there too (the platform layer is
# `crates/zenman-core/src/platform/windows.rs`); this script is the unix one because it is the one
# that is verified. See docs/windows-platform.md for the Windows build status.

set -Eeuo pipefail

PROFILE=release
RUN_CHECK=0
RUN_SMOKE=0
DO_INSTALL=0
DO_DIST=0
LOCKED=1
PREFIX="${PREFIX:-$HOME/.local}"

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)

BIN=zenman
PKG=zenman

die() {
    printf '\033[31merror:\033[0m %s\n' "$*" >&2
    exit 1
}
note() { printf '\033[36m==>\033[0m %s\n' "$*"; }
ok() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
warn() { printf '  \033[33mwarn\033[0m %s\n' "$*" >&2; }

usage() {
    cat <<'USAGE'
usage: scripts/build.sh [OPTION]...

  --check          run the preflight only (no fetch, no build)
  --debug          build the dev profile (target/debug/zenman) — faster link, slower binary
  --smoke          after building, boot the binary headless and exit (--safe-gpu --smoke 1)
  --install        install the binary to $PREFIX/bin (default ~/.local/bin)
  --dist           package target/dist/<name>.tar.gz (binary + README + ATTRIBUTION)
  --no-locked      let cargo re-resolve instead of honouring Cargo.lock
  -h, --help       this message

Environment:
  CARGO            cargo to use                (default: cargo on PATH)
  PREFIX           install prefix for --install (default: ~/.local)
  ZENMAN_SKIP_CC=1 silence the C-compiler check (only valid if nothing in the graph needs one)

examples:
  scripts/build.sh --check          # "can this machine build it?"
  scripts/build.sh                  # -> target/release/zenman
  scripts/build.sh --smoke          # ... and prove it boots
  scripts/build.sh --install        # -> ~/.local/bin/zenman
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)       RUN_CHECK=1 ;;
        --debug)       PROFILE=dev ;;
        --smoke)       RUN_SMOKE=1 ;;
        --install)     DO_INSTALL=1 ;;
        --dist)        DO_DIST=1 ;;
        --no-locked)   LOCKED=0 ;;
        -h|--help)     usage; exit 0 ;;
        *)             usage >&2; die "unknown option '$1'" ;;
    esac
    shift
done

CARGO=${CARGO:-cargo}

# ----------------------------------------------------------------------------------------------
# 1. preflight
# ----------------------------------------------------------------------------------------------
# Every check here is "is this machine able to produce the artefact at all", and each one names the
# command that fixes it. Version numbers are read out of the repo (Cargo.toml's `rust-version`), never
# duplicated here, so the script cannot drift behind the MSRV.

preflight() {
    note "preflight ($ROOT)"

    if ! command -v "$CARGO" >/dev/null 2>&1; then
        die "cargo not found. Install rustup: https://rustup.rs (then \`rustup toolchain install stable\`)"
    fi
    ok "cargo $("$CARGO" --version | cut -d' ' -f2)"

    local need_rustc have
    need_rustc=$(sed -n 's/^rust-version = "\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -1)
    have=$(rustc --version 2>/dev/null | awk '{print $2}')
    [[ -n "$have" ]] || die "rustc not found (cargo is on PATH but its toolchain is broken); re-run \`rustup toolchain install stable\`"
    if [[ -n "$need_rustc" ]] && [[ "$(printf '%s\n%s\n' "$need_rustc" "$have" | sort -V | head -1)" != "$need_rustc" ]]; then
        die "rustc $have is older than the declared MSRV $need_rustc — \`rustup update stable\`"
    fi
    ok "rustc $have (MSRV $need_rustc)"

    # rust-toolchain.toml pins `stable` and names rustfmt/clippy; `xtask ci` is unusable without them,
    # so a missing component is worth one line now rather than a gate failure later.
    if command -v rustup >/dev/null 2>&1; then
        local missing=()
        for c in rustfmt clippy; do
            rustup component list --installed 2>/dev/null | grep -q "^$c-" || missing+=("$c")
        done
        if ((${#missing[@]})); then
            warn "toolchain components missing: ${missing[*]} — \`rustup component add ${missing[*]}\` (needed by \`cargo xtask ci\`)"
        else
            ok "components rustfmt, clippy"
        fi
    fi

    # `rusqlite`'s `bundled` feature (§5.1: the reason the build does not depend on a distro libsqlite3
    # that may lack FTS5) compiles the SQLite amalgamation, so `cc` has to exist even though this is
    # "just Rust". The linker too — which is the failure mode that looks like a Rust bug.
    if [[ ${ZENMAN_SKIP_CC:-0} != 1 ]]; then
        if ! command -v "${CC:-cc}" >/dev/null 2>&1; then
            die "no C compiler: libsqlite3-sys compiles the SQLite amalgamation (bundled, FTS5) and needs one"
        fi
        ok "cc: $(command -v "${CC:-cc}")"
    else
        warn "C-compiler check skipped (ZENMAN_SKIP_CC=1)"
    fi

    # The dependency graph resolves the egui/winit/craft-libs forks over https; Cargo.lock pins them by
    # rev, so a build is reproducible only while those revs are reachable.
    if [[ ! -s "$ROOT/Cargo.lock" ]]; then
        warn "Cargo.lock missing — the build will resolve fresh, which is not what the pins were audited against"
    fi

    local free_kb
    free_kb=$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
    [[ -n "$free_kb" ]] && ((free_kb > 4 * 1024 * 1024)) \
        && ok "disk: $((free_kb / 1024 / 1024)) GB free" \
        || warn "less than 4 GB free on $ROOT (a full release target dir is ~2 GB)"

    ok "preflight passed"
}

preflight

if ((RUN_CHECK)); then
    exit 0
fi

# ----------------------------------------------------------------------------------------------
# 2. fetch
# ----------------------------------------------------------------------------------------------
cd "$ROOT"

if ((LOCKED)); then
    note "fetching locked dependencies"
    "$CARGO" fetch --locked
else
    note "fetching dependencies (--no-locked: resolving, Cargo.lock may change)"
    "$CARGO" fetch
fi

# ----------------------------------------------------------------------------------------------
# 3. build
# ----------------------------------------------------------------------------------------------
build_args=(build -p "$PKG")
if [[ $PROFILE == release ]]; then
    build_args+=(--release)
fi
((LOCKED)) && build_args+=(--locked)

note "cargo ${build_args[*]}"
time "$CARGO" "${build_args[@]}"

ARTIFACT="$ROOT/target/$PROFILE/$BIN"
[[ -x "$ARTIFACT" ]] || die "build reported success but $ARTIFACT is missing"

sha=$(sha256sum "$ARTIFACT" 2>/dev/null | cut -c1-12 || shasum -a 256 "$ARTIFACT" | cut -c1-12)
ok "$ARTIFACT  ($(du -h "$ARTIFACT" | cut -f1), sha256:${sha})"
"$ARTIFACT" --version

# ----------------------------------------------------------------------------------------------
# 4. smoke / install / dist
# ----------------------------------------------------------------------------------------------
if ((RUN_SMOKE)); then
    # The only sanctioned window test (README): a real boot, a real GPU, a window that closes itself.
    smoke=("$ARTIFACT" --safe-gpu --smoke 1 --no-daemon)
    if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
        command -v xvfb-run >/dev/null || die "no display and no xvfb-run: cannot run --smoke"
        smoke=(xvfb-run -a "${smoke[@]}")
    fi
    note "smoke: ${smoke[*]}"
    "${smoke[@]}"
    ok "booted and exited 0"
fi

if ((DO_INSTALL)); then
    install -D -m 0755 "$ARTIFACT" "$PREFIX/bin/$BIN"
    ok "installed $PREFIX/bin/$BIN"
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) warn "$PREFIX/bin is not on PATH" ;;
    esac
fi

if ((DO_DIST)); then
    # A tarball of the thing itself plus the attribution the licence gate requires (docs/contracts.md,
    # ATTRIBUTION.md): fonts and icons ship embedded in the binary, so their credits ship with it.
    host=$(rustc -vV | sed -n 's/^host: //p')
    # The version the binary itself reports: `[workspace.package] version`, which apps/zenman inherits
    # (`version.workspace = true`). Parsed from the manifest rather than hardcoded here.
    version=$(sed -n '/^\[workspace.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
        | sed -n 's/^version = "\([^"]*\)".*/\1/p' | head -1)
    [[ -n "$version" ]] || version=0
    name="zenman-$version-$host"
    out="$ROOT/target/dist"
    staging=$(mktemp -d)
    trap 'rm -rf "$staging"' EXIT
    mkdir -p "$staging/$name/bin"
    cp "$ARTIFACT" "$staging/$name/bin/$BIN"
    cp "$ROOT/README.md" "$ROOT/ATTRIBUTION.md" "$staging/$name/"
    # Both texts or neither: the workspace declares `MIT OR Apache-2.0`, so a one-sided package would
    # misstate what the recipient has. Absent files are a licence-gate gap, not this script's to paper
    # over — it says so and keeps building.
    if [[ -f "$ROOT/LICENSE-MIT" && -f "$ROOT/LICENSE-APACHE" ]]; then
        cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$staging/$name/"
    else
        warn "LICENSE-MIT / LICENSE-APACHE missing at the repo root — not packaged, but the crate declares 'MIT OR Apache-2.0'"
    fi
    mkdir -p "$out"
    tar -C "$staging" -czf "$out/$name.tar.gz" "$name"
    rm -rf "$staging"; trap - EXIT
    ok "$out/$name.tar.gz  ($(du -h "$out/$name.tar.gz" | cut -f1))"
fi
