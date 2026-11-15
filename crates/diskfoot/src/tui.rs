//! Terminal setup and the event loop. The model lives in [`crate::app`].

use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::app::App;
use crate::error::{Error, Result};
use crate::walk::Options;

pub fn run(root: &Path, options: Options) -> Result<()> {
    enable_raw_mode().map_err(|error| Error::io(Path::new("<terminal>"), error))?;
    let mut stdout = std::io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(Error::io(Path::new("<terminal>"), error));
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|error| Error::io(Path::new("<terminal>"), error))?;

    let result = event_loop(&mut terminal, root, options);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, root: &Path, options: Options) -> Result<()> {
    let mut app = App::new(root, options);
    loop {
        app.poll();
        terminal
            .draw(|frame| app.render(frame))
            .map_err(|error| Error::io(Path::new("<terminal>"), error))?;

        let available = event::poll(Duration::from_millis(50))
            .map_err(|error| Error::io(Path::new("<terminal>"), error))?;
        if !available {
            continue;
        }
        match event::read().map_err(|error| Error::io(Path::new("<terminal>"), error))? {
            Event::Key(key) if key.kind == KeyEventKind::Press && !app.on_key(key) => {
                return Ok(());
            }
            _ => {}
        }
    }
}
