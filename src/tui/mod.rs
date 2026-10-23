//! Terminal UI: async event loop over key events, background job results and a tick.

pub mod actions;
pub mod actions_admin;
pub mod actions_cv;
pub mod app;
pub mod form;
pub mod kinds;
pub mod tasks;
pub mod view;

use app::{App, Screen};
use futures::StreamExt;
use kinds::Kind;
use ratatui::crossterm::event::{Event, EventStream, KeyEventKind};
use std::time::Duration;

/// Run the TUI until the operator quits. `ratatui::init` installs a panic hook that
/// restores the terminal; SIGTERM/SIGHUP also exit cleanly.
pub async fn run(mut app: App, profile: Option<String>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let mut events = EventStream::new();
    let mut rx = app.rx.take().expect("receiver");
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    #[cfg(unix)]
    let (mut term, mut hup) = {
        use tokio::signal::unix::{SignalKind, signal};
        (signal(SignalKind::terminate())?, signal(SignalKind::hangup())?)
    };
    match profile {
        Some(p) => {
            app.connect(&p);
            app.open(Screen::Dashboard);
        }
        None => app.open(Screen::List(Kind::Profiles)),
    }
    let res: anyhow::Result<()> = async {
        loop {
            terminal.draw(|f| view::draw(f, &app))?;
            #[cfg(unix)]
            let signal = async {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = hup.recv() => {}
                }
            };
            #[cfg(not(unix))]
            let signal = std::future::pending::<()>();
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => app.handle_key(k),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
                Some(m) = rx.recv() => {
                    app.handle_msg(m);
                    // Drain bursts so many task updates cost one redraw.
                    while let Ok(m) = rx.try_recv() {
                        app.handle_msg(m);
                    }
                }
                _ = tick.tick() => app.tick(),
                _ = signal => break,
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    }
    .await;
    ratatui::restore();
    res
}
