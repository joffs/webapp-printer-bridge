use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        // Resource id 1 is loaded at runtime for the tray icon (see tray.rs).
        res.set_icon_with_id("assets/icon.ico", "1");
        res.set_manifest_file("assets/app.manifest");
        res.set("ProductName", "WebApp Printer Bridge");
        res.set("FileDescription", "WebApp Printer Bridge in Rust");
        res.compile().expect("failed to compile Windows resources");
    }

    copy_pdfium();
}

/// Put the matching `vendor\pdfium\<arch>\bin\pdfium.dll` next to the built exe, examples and
/// test binaries, so `cargo run` / `cargo test` find it (see `printing::pdf::library_path`).
fn copy_pdfium() {
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        _ => return,
    };
    let dll = PathBuf::from(format!("vendor/pdfium/{arch}/bin/pdfium.dll"));
    println!("cargo:rerun-if-changed={}", dll.display());
    if !dll.is_file() {
        println!(
            "cargo:warning={} missing - run scripts\\fetch-pdfium.ps1 (PDF printing will be unavailable)",
            dll.display()
        );
        return;
    }

    // OUT_DIR = target\[<triple>\]<profile>\build\<pkg>-<hash>\out
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };
    for dir in [
        profile_dir.to_path_buf(),
        profile_dir.join("examples"),
        profile_dir.join("deps"),
    ] {
        if let Err(e) = copy_if_changed(&dll, &dir.join("pdfium.dll")) {
            // E.g. the DLL is locked by a running bridge.
            println!(
                "cargo:warning=Cannot copy pdfium.dll to {}: {e}",
                dir.display()
            );
        }
    }
}

fn copy_if_changed(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::read(to).is_ok_and(|existing| existing == std::fs::read(from).unwrap_or_default()) {
        return Ok(());
    }
    std::fs::create_dir_all(to.parent().unwrap())?;
    std::fs::copy(from, to).map(|_| ())
}
