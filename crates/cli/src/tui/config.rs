//! #70: the UI locale in the global config (`[ui] locale`).
//!
//! Saving edits the one line in place rather than re-serializing the
//! file, so a user's comments and layout survive; the edited text is
//! parsed back and written only if it still says exactly what was asked.

use std::{fs, path::Path};

/// `[ui] locale` as written, if the file exists and parses.
pub fn load(path: &Path) -> Option<String> {
    let table: toml::Table = toml::from_str(&fs::read_to_string(path).ok()?).ok()?;
    table.get("ui")?.get("locale")?.as_str().map(str::to_owned)
}

/// Set `[ui] locale = "<tag>"`. Never creates the file: an install that
/// has none has not been bootstrapped by the daemon yet.
pub fn save(path: &Path, tag: &str) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let edited = edit(&text, tag);
    let table: toml::Table = toml::from_str(&edited).map_err(|error| error.to_string())?;
    let written = table
        .get("ui")
        .and_then(|ui| ui.get("locale"))
        .and_then(toml::Value::as_str);
    if written != Some(tag) {
        return Err("the config's [ui] table has a shape this edit does not touch".to_owned());
    }
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, edited).map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn edit(text: &str, tag: &str) -> String {
    let setting = format!("locale = \"{tag}\"");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut ui_header = None;
    let mut in_ui = false;
    for (index, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_ui = trimmed == "[ui]";
            if in_ui {
                ui_header = Some(index);
            }
        } else if in_ui && trimmed.split('=').next().map(str::trim) == Some("locale") {
            *line = setting;
            return lines.join("\n") + "\n";
        }
    }
    match ui_header {
        Some(index) => lines.insert(index + 1, setting),
        None => {
            lines.push(String::new());
            lines.push("[ui]".to_owned());
            lines.push(setting);
        }
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_comments_and_replace_only_the_locale() {
        let appended = edit("# mine\nformat_version = 1\n", "ko");
        assert_eq!(
            appended,
            "# mine\nformat_version = 1\n\n[ui]\nlocale = \"ko\"\n"
        );
        assert_eq!(
            edit(&appended, "en"),
            "# mine\nformat_version = 1\n\n[ui]\nlocale = \"en\"\n"
        );
        assert_eq!(
            edit("format_version = 1\n[ui]\n# keep\n", "ko"),
            "format_version = 1\n[ui]\nlocale = \"ko\"\n# keep\n"
        );
    }

    #[test]
    fn save_round_trips_and_refuses_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("bp-tui-config-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("config.toml");
        let _ = fs::remove_file(&path);
        assert!(save(&path, "ko").is_err(), "never creates the file");

        fs::write(&path, "# user comment\nformat_version = 1\n").expect("config");
        save(&path, "ko").expect("save");
        assert_eq!(load(&path).as_deref(), Some("ko"));
        assert!(
            fs::read_to_string(&path)
                .expect("read")
                .starts_with("# user comment\n")
        );
        save(&path, "xx").expect("any tag is stored as written");
        assert_eq!(load(&path).as_deref(), Some("xx"));

        // A dotted key the line edit does not understand: refused, untouched.
        fs::write(&path, "format_version = 1\nui.locale = \"en\"\n").expect("config");
        assert!(save(&path, "ko").is_err());
        assert_eq!(load(&path).as_deref(), Some("en"));
        let _ = fs::remove_dir_all(&dir);
    }
}
