//! #70: `brainprint tui` -- a keyboard-first terminal view over the same
//! daemon queries the other commands use. The daemon is not owned here:
//! quitting restores the terminal and drops a connection, nothing else.

mod app;
mod config;
mod view;

#[cfg(test)]
mod tests;

use brainprint_core::{
    present::Locale,
    protocol::{EndpointPaths, endpoint::global_config_path},
};
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, Command, Daemon, perform};

pub async fn run(workspace: &str, locale: Option<String>) -> i32 {
    let endpoint = match EndpointPaths::resolve() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return 5;
        }
    };
    let config = global_config_path();
    let locale = locale
        .or_else(|| config.as_deref().and_then(config::load))
        .map_or(Locale::En, |tag| Locale::from_tag(&tag));
    // The same locator the other commands send for this path.
    let workspace = crate::query::absolute_workspace(workspace);
    let daemon = Daemon {
        endpoint,
        workspace,
        config,
    };

    let mut app = App::new(locale);
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("brainprint: cannot open the terminal: {error}");
            return 1;
        }
    };
    perform(&mut app, &daemon, Command::Refresh).await;
    let outcome = loop {
        if let Err(error) = terminal.draw(|frame| view::render(frame, &app)) {
            break Err(error);
        }
        if app.quit {
            break Ok(());
        }
        // A resize just redraws on the next turn.
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                if let Some(command) = app.on_key(key) {
                    perform(&mut app, &daemon, command).await;
                }
            }
            Ok(_) => {}
            Err(error) => break Err(error),
        }
    };
    ratatui::restore();
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("brainprint: terminal error: {error}");
            1
        }
    }
}
