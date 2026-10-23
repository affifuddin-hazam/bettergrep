//! Windowless build of the CLI, used to run the daemon at logon without a console.
#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(bettergrep::cli::main());
}
