fn main() {
    // tauri_build embeds the common-controls v6 manifest into the app binary
    // only; test binaries link tao's comctl32 imports (SetWindowSubclass et
    // al.), which v5 exports by ordinal alone — without this the loader kills
    // every `cargo test` exe on Windows with STATUS_ENTRYPOINT_NOT_FOUND.
    // rustc-link-arg-tests covers only [[test]] targets and this crate's
    // tests live in the lib, so the arg goes on every link; SxS tolerates
    // the duplicate dependency this adds to the app binary's manifest.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             publicKeyToken='6595b64144ccf1df' language='*' \
             processorArchitecture='*'"
        );
    }
    tauri_build::build();
}
