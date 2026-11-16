// SPDX-License-Identifier: GPL-2.0

//! User program build script.
//!
//! Links the program as a HaisuliOS user executable at `USER_BASE` (see `user/user.ld`).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/../user.ld");
    // Position-independent code linked at USER_BASE: a plain static executable.
    println!("cargo:rustc-link-arg-bins=--no-pie");
    println!("cargo:rerun-if-changed=../user.ld");
}
