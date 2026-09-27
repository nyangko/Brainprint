//! Conservative read/search/list shell-form recognition (#26 "Native
//! exploration classification").
//!
//! This is deliberately NOT a shell parser and NOT command intelligence
//! (I6 owns that). It recognizes a handful of exact, single-command,
//! plain-token forms whose semantics map 1:1 onto a Brainprint request.
//! Everything else is either `UnprovenExploration` (a known exploration
//! program used in a form not proven here) or `Opaque` (not exploration
//! at all). Unknown => allowed, always.

use crate::event::{ActionClass, FallbackReason, NativeAction, SearchPattern};

/// Any of these makes the command's meaning depend on shell semantics
/// this module does not model (pipes, lists, redirection, expansion,
/// globbing, subshells, escapes).
const SHELL_META: &[char] = &[
    '|', ';', '&', '>', '<', '$', '`', '(', ')', '{', '}', '*', '?', '[', ']', '~', '!', '\\',
    '\n', '\r', '#',
];

pub fn classify(command: &str) -> NativeAction {
    let program = command.split_whitespace().next().unwrap_or_default();
    let exploration_class = exploration_class(program);

    let unproven = |reason| match exploration_class {
        Some(class) => NativeAction::UnprovenExploration { class, reason },
        None => NativeAction::Opaque,
    };

    if command.contains(SHELL_META) {
        return unproven(FallbackReason::UnprovenEquivalence);
    }
    let Some(tokens) = tokenize(command) else {
        return unproven(FallbackReason::UnprovenEquivalence);
    };
    let Some((program, args)) = tokens.split_first() else {
        return NativeAction::Inert;
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let classified = match program.as_str() {
        "cat" => cat(&args),
        "sed" => sed(&args),
        "head" => head(&args),
        "find" => find(&args),
        "rg" => rg(&args),
        _ => None,
    };
    classified.unwrap_or_else(|| unproven(FallbackReason::UnsupportedFlags))
}

fn exploration_class(program: &str) -> Option<ActionClass> {
    Some(match program {
        "cat" | "sed" | "head" | "tail" | "awk" | "less" | "more" | "nl" | "bat" | "strings"
        | "xxd" | "od" | "wc" => ActionClass::SourceRead,
        "rg" | "grep" | "egrep" | "fgrep" | "ag" | "ack" => ActionClass::TextSearch,
        "find" | "fd" | "ls" | "tree" | "du" => ActionClass::ProjectTreeDiscovery,
        _ => return None,
    })
}

/// Split plain tokens; accept `'...'` and `"..."` quoting only when the
/// quoted text is itself plain. `None` on anything ambiguous.
fn tokenize(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            '\'' | '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some(end) if end == ch => break,
                        Some(inner) => current.push(inner),
                        None => return None,
                    }
                }
            }
            other => {
                in_token = true;
                current.push(other);
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    // An environment assignment prefix (`FOO=1 rg ...`) changes the
    // program's behaviour in ways not modelled here.
    if tokens.first().is_some_and(|first| first.contains('=')) {
        return None;
    }
    Some(tokens)
}

fn is_flag(token: &str) -> bool {
    token.starts_with('-')
}

/// `cat <file>` -- exactly one operand, no flags.
fn cat(args: &[&str]) -> Option<NativeAction> {
    match args {
        [path] if !is_flag(path) => Some(NativeAction::SourceRead {
            path: (*path).to_owned(),
            lines: None,
        }),
        _ => None,
    }
}

/// `sed -n 'A,Bp' <file>` / `sed -n 'Ap' <file>` (1-based, inclusive).
fn sed(args: &[&str]) -> Option<NativeAction> {
    let ["-n", script, path] = args else {
        return None;
    };
    if is_flag(path) {
        return None;
    }
    let range = script.strip_suffix('p')?;
    let (start, end) = match range.split_once(',') {
        Some((start, end)) => (start.parse::<usize>().ok()?, end.parse::<usize>().ok()?),
        None => {
            let line = range.parse::<usize>().ok()?;
            (line, line)
        }
    };
    if start == 0 || end < start {
        return None;
    }
    Some(NativeAction::SourceRead {
        path: (*path).to_owned(),
        lines: Some((start - 1, end)),
    })
}

/// `head <file>` (10 lines) / `head -n N <file>`.
fn head(args: &[&str]) -> Option<NativeAction> {
    let (count, path) = match args {
        [path] => (10, *path),
        ["-n", count, path] => (count.parse::<usize>().ok()?, *path),
        _ => return None,
    };
    if is_flag(path) || count == 0 {
        return None;
    }
    Some(NativeAction::SourceRead {
        path: path.to_owned(),
        lines: Some((0, count)),
    })
}

/// `find <dir> -type f` -- every file under one directory, nothing else.
fn find(args: &[&str]) -> Option<NativeAction> {
    match args {
        [dir, "-type", "f"] if !is_flag(dir) => Some(NativeAction::FileDiscovery {
            scope: Some((*dir).to_owned()),
        }),
        _ => None,
    }
}

/// `rg --files [dir]`, or `rg` in files-with-matches/count mode with
/// only `-F`/`-i` beside it. A user ripgrep config file can inject any
/// flag, so its presence makes every `rg` form unproven.
fn rg(args: &[&str]) -> Option<NativeAction> {
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|value| !value.is_empty()) {
        return None;
    }
    if let Some(rest) = args.strip_prefix(&["--files"]) {
        return match rest {
            [] => Some(NativeAction::FileDiscovery { scope: None }),
            [dir] if !is_flag(dir) => Some(NativeAction::FileDiscovery {
                scope: Some((*dir).to_owned()),
            }),
            _ => None,
        };
    }

    let mut fixed = false;
    let mut case_insensitive = false;
    let mut files_or_count = false;
    let mut operands = Vec::new();
    for arg in args {
        match *arg {
            "-l" | "--files-with-matches" | "-c" | "--count" => files_or_count = true,
            "-F" | "--fixed-strings" => fixed = true,
            "-i" | "--ignore-case" => case_insensitive = true,
            flag if is_flag(flag) => return None,
            operand => operands.push(operand),
        }
    }
    // Content mode prints lines/context Brainprint's match list does not
    // carry verbatim: not proven derivable.
    if !files_or_count {
        return Some(NativeAction::UnprovenExploration {
            class: ActionClass::TextSearch,
            reason: FallbackReason::UnprovenEquivalence,
        });
    }
    let (pattern, scope) = match operands.as_slice() {
        [pattern] => (*pattern, None),
        [pattern, dir] => (*pattern, Some((*dir).to_owned())),
        _ => return None,
    };
    Some(NativeAction::TextSearch {
        pattern: if fixed {
            SearchPattern::Literal(pattern.to_owned())
        } else {
            SearchPattern::Regex(pattern.to_owned())
        },
        case_insensitive,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unproven(class: ActionClass, reason: FallbackReason) -> NativeAction {
        NativeAction::UnprovenExploration { class, reason }
    }

    #[test]
    fn exact_read_forms() {
        assert_eq!(
            classify("cat src/lib.rs"),
            NativeAction::SourceRead {
                path: "src/lib.rs".into(),
                lines: None
            }
        );
        assert_eq!(
            classify("sed -n '10,20p' src/lib.rs"),
            NativeAction::SourceRead {
                path: "src/lib.rs".into(),
                lines: Some((9, 20))
            }
        );
        assert_eq!(
            classify("head -n 5 a.rs"),
            NativeAction::SourceRead {
                path: "a.rs".into(),
                lines: Some((0, 5))
            }
        );
    }

    #[test]
    fn exact_discovery_and_search_forms() {
        assert_eq!(
            classify("rg --files crates"),
            NativeAction::FileDiscovery {
                scope: Some("crates".into())
            }
        );
        assert_eq!(
            classify("find src -type f"),
            NativeAction::FileDiscovery {
                scope: Some("src".into())
            }
        );
        assert_eq!(
            classify("rg -l -F 'retry budget' src"),
            NativeAction::TextSearch {
                pattern: SearchPattern::Literal("retry budget".into()),
                case_insensitive: false,
                scope: Some("src".into())
            }
        );
    }

    #[test]
    fn complex_or_unknown_forms_are_unproven_or_opaque() {
        // Pipelines, lists, globbing, expansion.
        assert_eq!(
            classify("cat src/lib.rs | head"),
            unproven(ActionClass::SourceRead, FallbackReason::UnprovenEquivalence)
        );
        assert_eq!(
            classify("rg -l foo src/*.rs"),
            unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence)
        );
        assert_eq!(
            classify("cat \"$FILE\""),
            unproven(ActionClass::SourceRead, FallbackReason::UnprovenEquivalence)
        );
        // Unknown flags.
        assert_eq!(
            classify("rg -l --hidden foo"),
            unproven(ActionClass::TextSearch, FallbackReason::UnsupportedFlags)
        );
        assert_eq!(
            classify("cat -n a.rs"),
            unproven(ActionClass::SourceRead, FallbackReason::UnsupportedFlags)
        );
        // Content-mode search.
        assert_eq!(
            classify("rg foo src"),
            unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence)
        );
        // Unbalanced quoting.
        assert_eq!(
            classify("rg -l 'foo src"),
            unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence)
        );
        // Env prefix.
        assert_eq!(classify("LC_ALL=C rg -l foo"), NativeAction::Opaque);
        // Not exploration: never intercepted.
        assert_eq!(classify("cargo test --workspace"), NativeAction::Opaque);
        assert_eq!(classify("git commit -m x"), NativeAction::Opaque);
        assert_eq!(
            classify("ls -la"),
            unproven(
                ActionClass::ProjectTreeDiscovery,
                FallbackReason::UnsupportedFlags
            )
        );
    }
}
