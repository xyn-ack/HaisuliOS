// SPDX-License-Identifier: GPL-2.0

//! User program build script.
//!
//! Links the program as a HaisuliOS user executable at `USER_BASE` (see `user/user.ld`).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/../user.ld");
    // Position-independent code linked at USER_BASE: a plain static executable.
    println!("cargo:rustc-link-arg-bins=--no-pie");
    // Segments aligned to the 4 KiB pages of the kernel's loader (lld's default for arm64 is
    // 64 KiB, which would pad every program by that much).
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    println!("cargo:rerun-if-changed=../user.ld");
}
