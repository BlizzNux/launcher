// Prevents an extra console window on Windows; harmless elsewhere.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `blizznux --cli <command>` runs one engine command without a window (scripting, tests).
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--cli" {
        std::process::exit(blizznux_lib::cli(&args[2], &args[3..]));
    }
    blizznux_lib::run()
}
