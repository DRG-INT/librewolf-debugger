#!/bin/sh
# Build release bundles for every platform lwdbg is meant to run on.
#
# POSIX sh: no arrays, no `[[`, no `local`, no `$'...'`, no process
# substitution. Lists are carried in positional parameters via `set --`, which
# is the POSIX way and the only one that survives a path with a space in it.
#
# Three classes of target, and the difference decides what must be installed
# before this can do anything:
#
#   macos    the local toolchain. arm64 and x86_64 both build natively because
#            the Apple SDK carries both; lipo joins them into one binary.
#
#   cross    a container whose own architecture *is* the target's, so it is a
#            native build in there and no cross-toolchain exists to go wrong.
#            linux/amd64 runs under emulation on Apple Silicon: correct, slow.
#
#   remote   the target machine over ssh. The only route to
#            aarch64-pc-windows-msvc and to FreeBSD, neither of which has a
#            usable container image here.
#
# The GPU backend is off for every non-macOS build: wgpu drags platform
# graphics stacks into a cross build, the CPU path is the reference
# implementation anyway, and a vantage host measuring page loads has no use
# for a shader.

set -eu

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
DIST="$REPO/dist"
BINS='lwdbg lwdbg-host'

# Artifacts stay off the repo volume: its random-write latency starves rustc
# badly enough that one crate took over 13 minutes there, against 2m44s for
# the whole workspace on the internal disk.
TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/lwdbg-target}

# `cross` is unusable on this host: cross 0.2.5 publishes no Docker image for
# an aarch64-apple-darwin host, so it falls back to cargo on the host and then
# fails trying to install a Linux *toolchain*:
#   error: toolchain 'stable-x86_64-unknown-linux-gnu' may not be able to run
#          on this system
# Containers are still right, just driven directly with docker run.
#
# Docker mounts a directory into the container, so the source is mirrored to
# the fast disk and built from there; a project-relative target/ is then fast
# too.
CROSS_SRC=${CROSS_SRC:-$HOME/.cache/lwdbg-src}
RUST_IMAGE=${RUST_IMAGE:-rust:1-slim-bookworm}
# Bookworm's glibc 2.36 is the baseline on purpose: a binary linked against an
# older glibc runs on a newer one, not the other way round.
CARGO_VOL=lwdbg-cargo-registry

say() {
    printf '\n== %s\n' "$*"
}

container_runtime() {
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        printf 'docker\n'
        return 0
    fi
    if command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
        printf 'podman\n'
        return 0
    fi
    return 1
}

no_runtime_note() {
    printf '   skipped: no container runtime is reachable.\n' >&2
    printf '   On this machine:  colima start --cpu 4 --memory 4\n' >&2
    printf '   (8 GiB of RAM here in total, so --memory 10 is refused by the\n' >&2
    printf '    host, and colima start still exits 0 — check colima status.)\n' >&2
}

# ------------------------------------------------------------------- macos

native_macos() {
    say 'macOS universal (arm64 + x86_64), with the GPU backend'
    ( cd "$REPO" && cargo build --release --target aarch64-apple-darwin )
    ( cd "$REPO" && cargo build --release --target x86_64-apple-darwin )
    mkdir -p "$DIST/macos-universal"
    for b in $BINS
    do
        lipo -create -output "$DIST/macos-universal/$b" \
            "$TARGET_DIR/aarch64-apple-darwin/release/$b" \
            "$TARGET_DIR/x86_64-apple-darwin/release/$b"
        lipo -info "$DIST/macos-universal/$b"
    done
}

# ------------------------------------------------------------------- cross

sync_cross_src() {
    mkdir -p "$CROSS_SRC"
    rsync -a --delete \
        --exclude target --exclude dist --exclude .git --exclude .cargo \
        "$REPO/" "$CROSS_SRC/"
}

# container_build <triple> <platform> <dist-dir> <exe-ext> <apt pkgs> <pre-sh>
container_build() {
    cb_triple=$1
    cb_platform=$2
    cb_dir=$3
    cb_ext=${4:-}
    cb_apt=${5:-}
    cb_pre=${6:-}

    say "$cb_triple in a $cb_platform container (no GPU backend)"
    cb_rt=$(container_runtime) || { no_runtime_note; return 0; }

    cb_setup=''
    if [ -n "$cb_apt" ]; then
        cb_setup="apt-get update -qq && apt-get install -y -qq --no-install-recommends $cb_apt &&"
    fi
    if [ -n "$cb_pre" ]; then
        cb_setup="$cb_setup $cb_pre &&"
    fi

    # `sh -c`, not `bash -lc`: a login shell re-reads /etc/profile and drops
    # the image's PATH, which is how this first failed with
    # "cargo: command not found".
    if ! "$cb_rt" run --rm --platform "$cb_platform" \
            -v "$CROSS_SRC":/src -w /src \
            -v "$CARGO_VOL":/usr/local/cargo/registry \
            -e CARGO_TARGET_DIR=/src/target \
            -e CARGO_NET_GIT_FETCH_WITH_CLI=true \
            "$RUST_IMAGE" \
            sh -c "set -e
                   export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:\$PATH
                   $cb_setup
                   rustup target add $cb_triple >/dev/null 2>&1 || true
                   cargo build --release --target $cb_triple \
                     --no-default-features -p lwdbg -p lwdbg-host"
    then
        printf '   FAILED for %s\n' "$cb_triple" >&2
        return 0
    fi

    mkdir -p "$DIST/$cb_dir"
    for b in $BINS
    do
        cp "$CROSS_SRC/target/$cb_triple/release/$b$cb_ext" \
           "$DIST/$cb_dir/$b$cb_ext" || return 0
    done
    ls -l "$DIST/$cb_dir"
}

do_cross() {
    sync_cross_src

    container_build aarch64-unknown-linux-gnu linux/arm64 linux-aarch64-gnu '' \
        'pkg-config libssl-dev'
    container_build x86_64-unknown-linux-gnu linux/amd64 linux-x86_64-gnu '' \
        'pkg-config libssl-dev'

    # musl is statically linked, which is what a binary meant for an unknown
    # Linux wants. It needs musl-gcc for the C dependencies: libsqlite3-sys's
    # bundled SQLite and ring's asm.
    container_build x86_64-unknown-linux-musl linux/amd64 linux-x86_64-musl '' \
        'musl-tools musl-dev' \
        'export CC_x86_64_unknown_linux_musl=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc'
    container_build aarch64-unknown-linux-musl linux/arm64 linux-aarch64-musl '' \
        'musl-tools musl-dev' \
        'export CC_aarch64_unknown_linux_musl=musl-gcc CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc'

    container_build x86_64-pc-windows-gnu linux/amd64 windows-x86_64 .exe \
        'gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64'

    say 'x86_64-unknown-freebsd'
    printf '   not attempted: no FreeBSD container image and no sysroot here.\n'
    printf '   It is a remote target — build it on a FreeBSD host over ssh.\n'

    say 'aarch64-pc-windows-msvc'
    printf '   not attempted: no container image exists for an MSVC target.\n'
    printf '   Build it on the Windows machine (see the remote class).\n'
}

# ------------------------------------------------------------------ remote

# Build on the target machine itself. Needs key-based ssh — set up once with
# scripts/setup-ssh.sh — and a Rust toolchain there. No secret is read or
# written by this function.
remote_build() {
    rb_host=$1
    rb_dir=$2
    rb_ext=${3:-}
    rb_remote=${4:-lwdbg-src}

    say "$rb_host over ssh (no GPU backend)"
    if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "$rb_host" true 2>/dev/null; then
        printf '   skipped: %s is unreachable or has no key-based access\n' "$rb_host" >&2
        printf '   set it up once:  SSH_PASS=... scripts/setup-ssh.sh %s\n' "$rb_host" >&2
        return 0
    fi

    # shellcheck disable=SC2029  # $rb_remote is a local variable and must
    # expand here; the remote shell has no idea what it should be.
    ssh "$rb_host" "mkdir -p '$rb_remote'"
    # Source only. target/ and dist/ would be gigabytes and get rebuilt there.
    rsync -az --delete \
        --exclude target --exclude dist --exclude .git \
        "$REPO/" "$rb_host:$rb_remote/"
    # shellcheck disable=SC2029  # same: the directory is decided locally.
    ssh "$rb_host" "cd '$rb_remote' && cargo build --release \
        --no-default-features -p lwdbg -p lwdbg-host"

    mkdir -p "$DIST/$rb_dir"
    for b in $BINS
    do
        scp "$rb_host:$rb_remote/target/release/$b$rb_ext" "$DIST/$rb_dir/$b$rb_ext"
    done
    ls -l "$DIST/$rb_dir"
}

do_remote() {
    # Hosts come from ~/.ssh/config, so no address or credential lives here.
    # The Void box is the authoritative Linux build and the only route to
    # FreeBSD; the Windows box is the only route to the MSVC targets.
    remote_build "${VOID_HOST:-void}" linux-native
    remote_build "${WIN_HOST:-winlab}" windows-native .exe
}

# -------------------------------------------------------------------- main

case "${1:-all}" in
macos)  native_macos ;;
cross)  do_cross ;;
remote) do_remote ;;
all)    native_macos; do_cross; do_remote ;;
*)
    printf 'usage: %s [all|macos|cross|remote]\n' "$(basename -- "$0")" >&2
    exit 2
    ;;
esac

say "bundles in $DIST"
if [ -d "$DIST" ]; then
    find "$DIST" -type f -exec ls -l {} ';'
fi
