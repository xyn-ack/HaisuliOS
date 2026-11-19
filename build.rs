// SPDX-License-Identifier: MIT

//! Kernel build script.
//!
//! Links the kernel with its architecture's linker script: on x86_64, the Multiboot header first
//! at 1 MiB; on arm64, the Image header first, linked in the upper half.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "linker-aarch64.ld",
        _ => "linker.ld",
    };
    println!("cargo:rustc-link-arg-bins=-T{dir}/{script}");
    // Code linked at a fixed address: produce a plain static executable, not a static PIE.
    println!("cargo:rustc-link-arg-bins=--no-pie");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker-aarch64.ld");
}
