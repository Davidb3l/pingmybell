fn main() {
    let mut attributes = tauri_build::Attributes::new();
    // The Windows app manifest (Common Controls v6) is embedded by the LINKER
    // rather than by tauri-build. tauri-build links it through a resource that
    // only reaches BINARIES, so `cargo test`'s harness started without it and
    // died before running a single test with STATUS_ENTRYPOINT_NOT_FOUND —
    // comctl32 v6 exports (the dialog plugin's TaskDialogIndirect) resolve
    // only when the manifest asks for v6. CI never saw it because its Windows
    // job runs `cargo check`, which never links.
    //
    // `rustc-link-arg` (not `-bins`) applies to every artifact of this
    // package, tests included, and the shipped app gets the very same bytes.
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc {
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        let manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
    tauri_build::try_build(attributes).expect("tauri-build");
}
