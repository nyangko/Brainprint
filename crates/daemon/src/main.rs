use brainprint_core::BuildInfo;

const HELP: &str = "Brainprint daemon

Usage:
  brainprintd --help
  brainprintd --version
";

fn main() {
    match std::env::args().nth(1).as_deref() {
        None | Some("-h" | "--help") => println!("{HELP}"),
        Some("-V" | "--version") => {
            let build = BuildInfo::current();
            println!("brainprintd {}", build.version);
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }
}
