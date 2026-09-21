//! Pure CLI argument parsing (#15 task 10), kept separate from `main.rs`
//! so it is testable without an async runtime or a running daemon.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Install,
    Init { path: Option<String> },
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    Unknown(String),
    TooManyArguments,
}

/// Parse `argv` (already stripped of the program name, e.g.
/// `std::env::args().skip(1)`).
pub fn parse(args: &[String]) -> Result<Command, ArgError> {
    match args {
        [] => Ok(Command::Help),
        [first] if first == "-h" || first == "--help" => Ok(Command::Help),
        [first] if first == "-V" || first == "--version" => Ok(Command::Version),
        [first] if first == "install" => Ok(Command::Install),
        [first] if first == "status" => Ok(Command::Status),
        [first] if first == "init" => Ok(Command::Init { path: None }),
        [first, path] if first == "init" => Ok(Command::Init {
            path: Some(path.clone()),
        }),
        [first, ..] if first == "install" || first == "status" || first == "init" => {
            Err(ArgError::TooManyArguments)
        }
        [other, ..] => Err(ArgError::Unknown(other.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(parse(&args(&[])), Ok(Command::Help));
    }

    #[test]
    fn help_flags_are_recognized() {
        assert_eq!(parse(&args(&["-h"])), Ok(Command::Help));
        assert_eq!(parse(&args(&["--help"])), Ok(Command::Help));
    }

    #[test]
    fn version_flags_are_recognized() {
        assert_eq!(parse(&args(&["-V"])), Ok(Command::Version));
        assert_eq!(parse(&args(&["--version"])), Ok(Command::Version));
    }

    #[test]
    fn install_and_status_take_no_arguments() {
        assert_eq!(parse(&args(&["install"])), Ok(Command::Install));
        assert_eq!(parse(&args(&["status"])), Ok(Command::Status));
    }

    #[test]
    fn init_without_a_path_defaults_to_none() {
        assert_eq!(parse(&args(&["init"])), Ok(Command::Init { path: None }));
    }

    #[test]
    fn init_with_a_path_is_captured() {
        assert_eq!(
            parse(&args(&["init", "/repo/main"])),
            Ok(Command::Init {
                path: Some("/repo/main".to_owned())
            })
        );
    }

    #[test]
    fn extra_arguments_are_rejected() {
        assert_eq!(
            parse(&args(&["init", "/repo/main", "extra"])),
            Err(ArgError::TooManyArguments)
        );
        assert_eq!(
            parse(&args(&["status", "extra"])),
            Err(ArgError::TooManyArguments)
        );
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert_eq!(
            parse(&args(&["doctor"])),
            Err(ArgError::Unknown("doctor".to_owned()))
        );
    }
}
