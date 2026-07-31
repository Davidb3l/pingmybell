#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("install-claude") => std::process::exit(pingmybell::cli_install_claude()),
        Some("uninstall-claude") => std::process::exit(pingmybell::cli_uninstall_claude()),
        _ => pingmybell::run(),
    }
}
