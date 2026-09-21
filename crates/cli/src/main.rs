use brainprint_core::BuildInfo;

const HELP: &str = "Brainprint CLI

Usage:
  brainprint --help
  brainprint --version
";

fn main() {
    match std::env::args().nth(1).as_deref() {
        None | Some("-h" | "--help") => println!("{HELP}"),
        Some("-V" | "--version") => {
            let build = BuildInfo::current();
            println!("brainprint {}", build.version);
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }
}
