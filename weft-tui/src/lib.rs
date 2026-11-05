#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::io;

use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};
use tracing::info;

pub mod app;
pub mod logging;
mod num;
pub mod ui;

pub type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Runs the TUI until the user quits, restoring the terminal afterwards.
///
/// # Errors
///
/// Returns an error if the terminal cannot be set up or restored, or if drawing or reading
/// input fails.
pub async fn run(log_buffer: logging::LogBuffer) -> Result<()> {
	info!("Setting up terminal");
	let mut terminal = setup_terminal()?;

	info!("Initializing app");
	let mut app = app::App::new(log_buffer).await;

	info!("Starting app event loop");
	run_app(&mut terminal, &mut app).await?;

	info!("Restoring terminal");
	restore_terminal(&mut terminal)?;
	Ok(())
}

fn setup_terminal() -> Result<Tui> {
	let mut stdout = io::stdout();
	crossterm::terminal::enable_raw_mode()?;
	crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(stdout);
	let terminal = Terminal::new(backend)?;
	Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
	crossterm::terminal::disable_raw_mode()?;
	crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
	terminal.show_cursor()?;
	Ok(())
}

async fn run_app(terminal: &mut Tui, app: &mut app::App) -> Result<()> {
	loop {
		// Poll for background loading progress
		app.poll_loading();

		terminal.draw(|f| ui::draw(f, app))?;
		if crossterm::event::poll(std::time::Duration::from_millis(50))? {
			if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
				if app.handle_key(key).await? {
					break;
				}
			}
		}
	}
	Ok(())
}
