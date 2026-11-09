// SPDX-License-Identifier: GPL-2.0

//! Kernel build script.
//!
//! Links the kernel with its own linker script, which puts the Multiboot header first at 1 MiB.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    // Code is position independent (the target default) but linked at a fixed
    // address: produce a plain static executable, not a static PIE.
    println!("cargo:rustc-link-arg-bins=--no-pie");
    println!("cargo:rerun-if-changed=linker.ld");
}
