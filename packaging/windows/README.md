# Windows

## Moving caches off the C drive

`migrate-to-sata.zsh` moves the space-hungry user directories to
`C:\Sata(0x01)\` and leaves a link behind. Written for cmder's zsh.

```sh
./migrate-to-sata.zsh survey              # look, change nothing
./migrate-to-sata.zsh migrate --dry-run   # print every command
./migrate-to-sata.zsh migrate
```

Run `survey` first and read it: for each of the 20 candidates it prints the
size, whether `C:\Sata(0x01)\` already holds a copy, whether the two differ,
and whether a process that owns it is running. Two divergent copies are
refused rather than merged.

Three things make this go wrong, and the script handles each:

- **`ln -s` in cmder/MSYS2 silently *copies*** unless `winsymlinks` is
  configured. A 40 GB cache "linked" that way takes 80 GB and the halves
  drift. The script exports `MSYS=winsymlinks:nativestrict`, so `ln -s`
  either makes a real symlink or fails loudly.
- **A native symlink needs Developer Mode or elevation; a directory junction
  needs neither**, and every toolchain follows a junction while some refuse
  to traverse a symlink. So directories get `mklink /J` and only single files
  get `ln -s`.
- **For a cache with a documented environment variable, redirect instead of
  linking.** `CARGO_HOME`, `RUSTUP_HOME`, `GOMODCACHE`, `PIP_CACHE_DIR`,
  `GRADLE_USER_HOME`, `NUGET_PACKAGES`, `PNPM_HOME`, `npm_config_cache` and
  `DOTNET_CLI_HOME` are all supported by their tools; a junction merely works
  until something checks for a reparse point. The script emits the `setx`
  line for those and links only what has no variable.

It copies with `robocopy /MIR /COPYALL /DCOPY:DAT` rather than `move`
(restartable, keeps ACLs and timestamps), verifies the file count, and only
then removes the source. Nothing is touched while a process that owns it is
running: a junction swapped under an open handle loses writes.

## Building on Windows

Two targets matter here: `x86_64-pc-windows-gnu`, which cross-builds from a
Linux container, and `aarch64-pc-windows-msvc`, which **cannot** be
cross-built. No container image carries Microsoft's SDK, so the ARM64 MSVC
binary has to be built on the Windows machine.

## Toolchain

```powershell
winget install --id Rustlang.Rustup -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override `
  "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools `
   --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 `
   --add Microsoft.VisualStudio.Component.Windows11SDK.22621"
winget install --id Git.Git -e
winget install --id GnuWin32.Make -e     # for `make`, if you want the POSIX flow
```

Then:

```powershell
rustup default stable-aarch64-pc-windows-msvc
rustup target add aarch64-pc-windows-msvc x86_64-pc-windows-msvc
```

`VC.Tools.ARM64` is the component people miss. Without it the ARM64 link step
fails with a missing `link.exe` for the target architecture, which reads like a
broken rustup rather than a missing VS component.

## `ring` wants an assembler

`ring` (pulled in by rustls) builds assembly on Windows:

```powershell
winget install --id NASM.NASM -e
winget install --id StrawberryPerl.StrawberryPerl -e
```

For **x86_64-pc-windows-msvc** these are definitely needed. Whether
**aarch64-pc-windows-msvc** needs them too is **unverified** — check it rather
than assume:

```powershell
cargo build --release --no-default-features -p lwdbg -p lwdbg-host 2>&1 |
  Select-String -Pattern "nasm|perl|ring"
```

If that build succeeds with NASM uninstalled, aarch64 does not need it.

## Building

From an *x64 Native Tools* or *ARM64 Native Tools* prompt (the environment
matters; a plain PowerShell will not find `link.exe`):

```powershell
$env:LWDBG_DATADIR = "C:\Program Files\lwdbg\share"
cargo build --release --no-default-features -p lwdbg -p lwdbg-host
```

`--no-default-features` drops the wgpu backend. The CPU path is the reference
implementation, so `lwdbg parity` still works and reports `backend: cpu`.

## Being built from the Mac instead

With key-based ssh set up on this box, the whole thing is one command from the
other machine:

```sh
SSH_PASS='...' scripts/setup-ssh.sh winlab   # once
make bundle BUNDLE_WHAT=remote
```

That rsyncs the source over, builds there, and copies the binaries back into
`dist/windows-native/`. It needs OpenSSH Server running on the Windows side:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
```

The host is reached as `winlab` through `~/.ssh/config` on the Mac, on port
2222. Both machines were powered off when this was last attempted, and
`make bundle` reported `unreachable` rather than failing the run.

## What does not work on Windows

- `lwdbg proxy ca-install` expects a Gecko profile and `certutil`. NSS tools
  are not packaged separately on Windows; the CA has to be imported through
  the browser's own certificate manager.
- `lwdbg live` uses a Unix domain socket for the bridge. AF_UNIX exists on
  Windows 10+ but Rust's `std::os::unix::net` does not cover it, so the live
  layer is Unix-only for now. The proxy and the transports are unaffected.
- `lwdbg android` needs `adb` on PATH; `winget install Google.PlatformTools`.
