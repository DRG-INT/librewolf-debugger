# Building, installing, packaging

```sh
./configure --prefix=/usr/local
make
make check
make install                     # or: make DESTDIR=/tmp/stage install
make uninstall
```

POSIX throughout: `configure` and the helper scripts are POSIX `sh`, the
Makefile is POSIX make. No bash, no zsh, no autoconf. The reason is the target
machines — zsh is not in a FreeBSD base install, and anything a packager
touches has to work with `DESTDIR` and the system `make`.

## Why configure generates the Makefile

`configure` writes the whole `Makefile` from `Makefile.in` by substitution,
rather than emitting a `config.mk` that the Makefile includes.

The include directive is the one place make genuinely splits:
FreeBSD/NetBSD `bmake` wants `.include "config.mk"`, GNU make wants
`include config.mk`, and there is no spelling both accept. Substituting into a
template sidesteps the dialect entirely.

The Makefile itself sticks to POSIX so both `bmake` and the GNU make 3.81 that
macOS still ships can run it:

- macros assigned with `=` only; overriding is done the POSIX way, on the
  command line (`make PREFIX=/opt install`);
- no `ifeq`, no `$(shell ...)`, no `%` pattern rules — conditionals live in
  recipes;
- **no `install(1)`**. POSIX does not specify it, and GNU's `install -D` does
  not exist on FreeBSD. `mkdir -p`, `cp` and `chmod` are used instead.

## configure options

```text
--prefix=DIR --bindir=DIR --datarootdir=DIR --mandir=DIR --sysconfdir=DIR
--with-cargo=PATH --host=TRIPLE --target-dir=DIR
--with-gpu | --without-gpu       default: on for Darwin, off elsewhere
```

It checks four mandatory things — `cargo`, `rustc`, a C compiler, and that
`Makefile.in` exists — and then reports the optional ones with the exact
install command for whatever is missing. A **C compiler is mandatory**: `ring`,
`libsqlite3-sys` (bundled SQLite), `blake3` and `zstd-sys` all compile C, and
without one the build fails deep inside a build script with a confusing
message.

The target directory is resolved by `configure`, not left to cargo, because
the Makefile has to know where the binaries land. Order: `--target-dir`, then
whatever `.cargo/config.toml` already says, then `./target`. That config file
exists here on purpose: the archive volume starves `rustc` — one crate took
over **13 minutes** there at ~0.6% CPU, against **2m44s** for the whole
workspace on the internal disk.

## Do not re-run configure while make is running

`preflight` and `release` invoke sub-makes, and a sub-make **re-reads the
Makefile**. So running `./configure` mid-build changes the configuration under
the parts that have not started yet.

That is not hypothetical: a `./configure` run to check the memory warning
switched the GPU feature back on halfway through a `make preflight`, and the
clippy step then checked the whole wgpu tree on a machine with 8 GiB of RAM and
89% of its swap in use. The symptom was a gate that appeared to hang.

Configure, then build. If the configuration needs to change, stop the build
first.

## Two knobs for a small machine

```sh
make CARGO_PROFILE=release-lean     # no LTO, 16 codegen units
make AUDIT_GEIGER=1 audit           # re-run cargo geiger instead of reusing it
```

`release-lean` inherits `release` and gives up thin LTO, which is the build's
peak-memory moment once wgpu and naga are in the graph. The binary is slightly
larger and slightly slower; it exists, which the other one did not on this
machine. `release` stays the default because that is what a distributor should
ship.

`cargo geiger` builds the entire dependency tree in its own target directory —
several minutes — and its answer here is stable at 0/0 for both own crates, so
`audit` reuses the recorded result. A gate nobody runs because it takes twenty
minutes is not a gate.

`./configure` warns when the GPU feature is on and the machine has under
12 GiB: 8 demonstrably fails and 16 demonstrably works, so the message says
"may" for what is in between.

## install layout, and how the binary finds it

```text
$(DESTDIR)$(BINDIR)/lwdbg
$(DESTDIR)$(BINDIR)/lwdbg-host
$(DESTDIR)$(PREFIX)/share/lwdbg/skills/librewolf-debugger/SKILL.md
$(DESTDIR)$(PREFIX)/share/lwdbg/skills/librewolf-debugger/references/*.md
$(DESTDIR)$(PREFIX)/share/lwdbg/extension/{manifest.json,background.js}
$(DESTDIR)$(PREFIX)/share/lwdbg/ssot/schema.sql
$(DESTDIR)$(MANDIR)/man1/lwdbg.1
```

`cli/build.rs` bakes `$(PREFIX)/share/lwdbg` into the binary from the
`LWDBG_DATADIR` the Makefile passes, with `rerun-if-env-changed` so a changed
`--prefix` rebuilds instead of silently keeping the old path. The runtime
search order is: `$LWDBG_SKILL`, the baked path, paths relative to the
executable, the current directory. For a plain `cargo build` from the repo the
baked value is empty and nothing changes.

The install layout mirrors the repo on purpose — `extension/` sits beside
`skills/` in both — so one derivation in `data_root()` serves both and no code
has to know which it is looking at.

`install` writes `packaging/MANIFEST`, paths relative to `DESTDIR`, and
`uninstall` removes exactly those and then only the directories left empty.
`rmdir` without `-p` and with `|| true`, so a directory still holding somebody
else's files is kept rather than forced. The store under
`~/.local/share/lwdbg` is never touched: it holds captured traces and the local
CA, which are user data.

## Packages per platform — with the verification command

A package name I have not run is given as a query, not an assertion.

| | Build | `proxy ca-install` | `android` |
| --- | --- | --- | --- |
| **FreeBSD** | `pkg install rust` (clang is base) | `pkg install nss` — **verified**, the port ships `bin/certutil` | `pkg install android-tools` |
| **Void** | `xbps-install -S rust cargo base-devel pkg-config` | `nss` — **unverified** | **unverified** |
| **Debian** | `apt install cargo rustc build-essential pkg-config` | `apt install libnss3-tools` — Debian **does** split certutil out of `libnss3` | `apt install android-tools-adb` |
| **macOS** | `rustup`, `xcode-select --install` | `brew install nss` | `brew install --cask android-platform-tools` |
| **Windows** | `rustup` + VS Build Tools with `VC.Tools.ARM64` | n/a, import through the browser | `winget install Google.PlatformTools` |

Verify before trusting:

```sh
pkg info -l nss | grep certutil           # FreeBSD
xbps-query -Rs nss ; xbps-query -Rf nss | grep certutil   # Void
dpkg -L libnss3-tools | grep certutil     # Debian
brew list nss | grep certutil             # macOS
```

Target-specific extras that bite:

- **static musl**: `musl-tools` and `musl-dev`, plus
  `CC_<triple>=musl-gcc` and `CARGO_TARGET_<TRIPLE>_LINKER=musl-gcc`, because
  `libsqlite3-sys` and `ring` compile C.
- **`x86_64-pc-windows-gnu`**: `gcc-mingw-w64-x86-64`.
- **Windows MSVC**: `ring` wants `nasm` and `perl` on x86_64. Whether
  **aarch64** needs them is **unverified** — `packaging/windows/README.md` has
  the one-line check rather than a claim.
- the GPU feature is off everywhere except macOS, so **no Vulkan or X11
  headers are needed on Linux at all**.

## Bundles for other platforms

```sh
make bundle BUNDLE_WHAT=macos     # universal arm64 + x86_64, with the GPU backend
make bundle BUNDLE_WHAT=cross     # Linux gnu/musl both arches, Windows gnu
make bundle BUNDLE_WHAT=remote    # built on the target machine over ssh
```

`scripts/bundle.sh` drives it. Three facts it encodes, each found the hard way:

- **`cross` is unusable on Apple Silicon.** cross 0.2.5 publishes no Docker
  image for an `aarch64-apple-darwin` host, so it falls back to cargo on the
  host and then fails trying to install a Linux *toolchain*:
  `error: toolchain 'stable-x86_64-unknown-linux-gnu' may not be able to run
  on this system`. Containers are driven directly with `docker run` instead,
  each one's architecture matching its target so the build inside is native.
- inside `rust:1-slim-bookworm` use `sh -c`, **not** `bash -lc`: a login shell
  re-reads `/etc/profile` and drops the image's PATH, which is how this first
  failed with `cargo: command not found`.
- `colima start --memory 10` is **refused** on this 8 GiB Mac
  (`"memorySize" is greater than "maximumAllowedMemorySize"`), and
  `colima start` still exits 0 — check `colima status`, not `$?`.
  `colima start --cpu 4 --memory 4` works. Stop it when idle; 4 GiB of 8 is a
  lot to leave parked.

Bookworm's glibc 2.36 is the deliberate baseline: a binary linked against an
older glibc runs on a newer one, not the other way round.

**Not cross-buildable here**, and why:

- `x86_64-unknown-freebsd` — no container image, no libc sysroot. Build it on
  a FreeBSD host.
- `aarch64-pc-windows-msvc` — no image carries Microsoft's SDK. Build it on
  the Windows machine.

## ssh build hosts

`~/.ssh/config` carries `void` and `winlab` with addresses and usernames only;
no password lives there, and none is needed after the key is installed:

```sh
SSH_PASS='...' scripts/setup-ssh.sh void
```

That script drives `ssh-copy-id` with `expect`, reads the password from the
environment, and writes it nowhere. `sshpass` is not used — Homebrew refuses to
package it, and `expect` is in the FreeBSD base and a one-line add elsewhere.

Everything afterwards uses `BatchMode=yes`, so a host that is off fails fast
instead of hanging on a prompt. `make bundle BUNDLE_WHAT=remote` skips an
unreachable host with a line saying so rather than failing the run — which is
what it did when both machines were powered off.

## Native packaging skeletons

Real installs on those machines should go through the system packager, not
`make install` into `/usr/local` behind its back:

- `packaging/freebsd/Makefile` — ports skeleton, `USES=cargo`. `USES=cargo`
  rather than calling this Makefile because ports needs every crate listed in
  advance so it can fetch into `DISTDIR` and build with the network off.
  `make cargo-crates` on a FreeBSD box generates the ~250-entry
  `CARGO_CRATES` block; it is not pasted in, because it would be stale the
  first time a dependency moved.
  Note: GNU make cannot parse this file at all — `.include <bsd.port.mk>` is
  bmake syntax, and `missing separator` from `gmake` is the expected result.
- `packaging/void/template` — `xbps-src` template, `build_style=cargo`.
- `packaging/windows/README.md` — `winget` lines, the `VC.Tools.ARM64`
  component people miss, and what does not work on Windows at all.

## Verifying

```sh
# POSIX conformance of the scripts, not just "it ran"
for f in configure scripts/*.sh evals/run.sh; do sh -n "$f"; dash -n "$f"; done

# a staged install, which is what a packager does
./configure --prefix=/usr/local
make
make DESTDIR=/tmp/stage install
find /tmp/stage -type f | sort      # compare against packaging/MANIFEST
make DESTDIR=/tmp/stage uninstall
find /tmp/stage -type f             # must come back empty

# the installed binary finds its skill tree with no environment help
env -u LWDBG_SKILL /tmp/stage/usr/local/bin/lwdbg adapters list

# the man page
mandoc -T lint doc/lwdbg.1
man ./doc/lwdbg.1
```

For a remote target the only real verification is running it there:

```sh
scp dist/linux-x86_64-musl/lwdbg void:/tmp/ && ssh void '/tmp/lwdbg doctor --json'
```

`doctor` is the right smoke test: it exercises process spawning, filesystem
paths and the store, and reports what is missing instead of failing obscurely.
