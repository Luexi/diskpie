//! Embeds the Windows PE identity of the portable `diskpie.exe`.
//!
//! On `target_os = "windows"` this build script compiles one resource file
//! that carries the application manifest (`diskpie.manifest`), the icon group
//! from `assets/brand/diskpie-icon.ico`, and a `VERSIONINFO` block derived
//! from the package metadata. Every other target is a deliberate no-op so the
//! portable crates and the Linux CI job never need a Windows SDK.
//!
//! The resource contract itself is documented in
//! `docs/research/windows-packaging-and-release.md`.

#![forbid(unsafe_code)]

use std::env;
use std::path::Path;

/// Manifest embedded as resource ID 1, relative to this crate's manifest dir.
const MANIFEST_FILE: &str = "diskpie.manifest";

/// Multi-resolution icon embedded as the `RT_GROUP_ICON` resource.
const ICON_FILE: &str = "../../assets/brand/diskpie-icon.ico";

/// Copyright holder and year, matching `LICENSE-MIT` at the repository root.
const LEGAL_COPYRIGHT: &str = "Copyright (c) 2026 Luis";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    println!("cargo:rerun-if-changed={MANIFEST_FILE}");
    println!("cargo:rerun-if-changed={ICON_FILE}");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .expect("cargo always sets CARGO_MANIFEST_DIR for build scripts");
    let manifest_dir = Path::new(&manifest_dir);
    let manifest_path = manifest_dir.join(MANIFEST_FILE);
    let icon_path = manifest_dir.join(ICON_FILE);

    let version = env::var("CARGO_PKG_VERSION").expect("cargo always sets CARGO_PKG_VERSION");
    let description =
        env::var("CARGO_PKG_DESCRIPTION").expect("cargo always sets CARGO_PKG_DESCRIPTION");
    let authors = env::var("CARGO_PKG_AUTHORS").expect("cargo always sets CARGO_PKG_AUTHORS");
    let company = authors.split(':').next().unwrap_or_default().trim();

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_manifest_file(&manifest_path.to_string_lossy())
        .set_icon(&icon_path.to_string_lossy())
        .set("ProductName", "DiskPie")
        .set("InternalName", "diskpie")
        .set("OriginalFilename", "diskpie.exe")
        .set("FileDescription", &description)
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("CompanyName", company)
        .set("LegalCopyright", LEGAL_COPYRIGHT);

    if let Err(error) = resource.compile() {
        // The resource compiler error is a build-time diagnostic; it may name
        // toolchain files but never user data.
        panic!("failed to compile the Windows resources for diskpie.exe: {error}");
    }
}
