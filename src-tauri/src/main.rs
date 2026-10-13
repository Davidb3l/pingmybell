#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    let arg = std::env::args().nth(1);
    // Headless subcommands print to the terminal that ran them. A
    // windows-subsystem build has no console of its own, so attach to the
    // parent's first. GUI launches skip this and never touch the console.
    if let Some(command) = pingmybell::cli_subcommand(arg.as_deref()) {
        pingmybell::attach_parent_console();
        std::process::exit(command());
    }
    pingmybell::run()
}
