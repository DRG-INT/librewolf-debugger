//! Bake the installed data directory into the binary.
//!
//! `make install` puts the skill tree and the extension under
//! `$(PREFIX)/share/lwdbg`, which nothing in the binary could otherwise find:
//! the runtime search looks at `$LWDBG_SKILL`, then paths relative to the
//! executable, then the current directory, and an installed `/usr/local/bin`
//! binary matches none of them.
//!
//! For a plain `cargo build` from the repository the variable is unset and the
//! baked value is empty, so the existing search is unchanged.

fn main() {
    // Without this, changing --prefix and re-running make would keep the old
    // path compiled in, and the failure would look like a missing install.
    println!("cargo:rerun-if-env-changed=LWDBG_DATADIR");
    let dir = std::env::var("LWDBG_DATADIR").unwrap_or_default();
    println!("cargo:rustc-env=LWDBG_DATADIR={dir}");
}
