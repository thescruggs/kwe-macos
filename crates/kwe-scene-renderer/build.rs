// SPDX-License-Identifier: Apache-2.0
// Compiles the vendored stb_truetype C shim (M3e text rasterizer).
// See vendor/stb/stb_shim.c and THIRD_PARTY.yml for provenance.

fn main() {
    let vendor = std::path::Path::new("vendor/stb");
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("stb_shim.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("stb_truetype.h").display()
    );
    cc::Build::new()
        .file(vendor.join("stb_shim.c"))
        .include(vendor)
        .warnings(false)
        .compile("kwe_stb_truetype");
}
