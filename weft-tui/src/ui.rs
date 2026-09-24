use chrono::{DateTime, Utc, Datelike, Timelike};
use database::database::traits::AspectStructure;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect}, style::{Color, Modifier, Style}, widgets::{Block, Borders, Chart, Dataset, GraphType, List, ListItem, Paragraph}, Frame
};
use splimes::Resolution;

use crate::app::{App, AppState, CompressionField, CompressionMode, CompressionStatus};

/// Split area into main content and log pane
fn split_with_logs(area: Rect) -> (Rect, Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(0),    // Main content
			Constraint::Length(8), // Log pane
		])
		.split(area);
	(chunks[0], chunks[1])
}

/// Render the log pane
fn render_logs(f: &mut Frame, app: &App, area: Rect) {
	let messages = app.log_buffer.get_messages();
	let log_text = if messages.is_empty() { "[No logs yet]".to_string() } else { messages.join("\n") };

	let log_pane = Paragraph::new(log_text).block(Block::default().borders(Borders::TOP).title("Logs")).style(Style::default().fg(Color::Gray));
	f.render_widget(log_pane, area);
}

pub fn draw(f: &mut Frame, app: &App) {
	let area = f.area();
	let (main_area, log_area) = split_with_logs(area);

	match &app.state {
		AppState::SelectingMode => draw_mode_selection(f, app, main_area),
		AppState::EnteringDatabaseName { input, cursor_position, validation_error } => draw_database_name_input(f, input, *cursor_position, validation_error.as_deref(), main_area),
		AppState::CreatingDatabase { name } => draw_creating_database(f, name, main_area),
		AppState::EnteringSubjectName { input, cursor_position, validation_error } => draw_subject_name_input(f, input, *cursor_position, validation_error.as_deref(), main_area),
		AppState::CreatingSubject { name } => draw_creating_subject(f, name, main_area),
		AppState::EnteringAspectName { input, cursor_position, validation_error, selected_resolution } => draw_aspect_name_input(f, input, *cursor_position, validation_error.as_deref(), *selected_resolution, main_area),
		AppState::CreatingAspect { name, resolution: _ } => draw_creating_aspect(f, name, main_area),
		AppState::EnteringCsvPath { input, cursor_position, validation_error } => draw_csv_path_input(f, input, *cursor_position, validation_error.as_deref(), main_area),
		AppState::SelectingDatabase => draw_database_selection(f, app, main_area),
		AppState::SelectingSubject => draw_subject_selection(f, app, main_area),
		AppState::SelectingAspect => draw_aspect_selection(f, app, main_area),
		AppState::Loading { loaded_count, total_scanned, status } => {
			// Show compression pane during loading if compression is active
			if matches!(app.compression_status, Some(CompressionStatus::Running { .. })) {
				draw_loading_with_compression(f, app, *loaded_count, *total_scanned, status, main_area);
			} else {
				draw_loading(f, *loaded_count, *total_scanned, status, main_area);
			}
		}
		AppState::Plotting => {
			// Always show compression pane when plotting (shows last compression stats when idle)
			if let Some((loaded, total, status)) = &app.csv_import_progress {
				draw_plot_with_progress_and_compression(f, app, *loaded, *total, status, main_area);
			} else {
				draw_plot_with_compression_pane(f, app, main_area);
			}
		}
		AppState::SelectingCompressionMode { selected_index } => draw_compression_mode_selection(f, app, *selected_index, main_area),
		AppState::ConfiguringCompression { mode, time_pure_days, time_tier_days, time_max_tiers, time_scaling_index, size_target_gb, size_min_agg, size_max_agg, base_resolution_index, focused_field, validation_error } => {
			draw_compression_config(f, mode, time_pure_days, time_tier_days, time_max_tiers, *time_scaling_index, size_target_gb, size_min_agg, size_max_agg, *base_resolution_index, focused_field, validation_error.as_deref(), main_area);
		}
		AppState::Error(msg) => draw_error(f, msg, main_area),
	}

	// Always render logs at the bottom
	render_logs(f, app, log_area);
}

fn draw_mode_selection(f: &mut Frame, app: &App, area: Rect) {
	let items: Vec<ListItem> = vec![ListItem::new("Create New Database"), ListItem::new("Load Existing Database")];

	let list = List::new(items).block(Block::default().borders(Borders::ALL).title("WeftDB Database Manager - Select Mode")).highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol(">> ");

	f.render_stateful_widget(list, area, &mut app.mode_list_state.clone());
}

fn draw_database_name_input(f: &mut Frame, input: &str, cursor: usize, error: Option<&str>, area: Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3), // Title
			Constraint::Length(3), // Input
			Constraint::Length(3), // Error
			Constraint::Min(5),    // Instructions
		])
		.split(area);

	// Title
	let title = Paragraph::new("Create New Database").block(Block::default().borders(Borders::ALL));
	f.render_widget(title, chunks[0]);

	// Input with cursor
	let display = if cursor < input.len() { format!("{}|{}", &input[..cursor], &input[cursor..]) } else { format!("{}|", input) };
	let input_box = Paragraph::new(display).block(Block::default().borders(Borders::ALL).title("Database Name"));
	f.render_widget(input_box, chunks[1]);

	// Error
	if let Some(err) = error {
		let error_box = Paragraph::new(err).block(Block::default().borders(Borders::ALL)).style(Style::default().fg(Color::Red));
		f.render_widget(error_box, chunks[2]);
	}

	// Instructions
	let help = "Letters, numbers, underscore, hyphen only\n\
                Enter: Create | Esc: Back | Q: Quit";
	let help_box = Paragraph::new(help).block(Block::default().borders(Borders::ALL).title("Instructions"));
	f.render_widget(help_box, chunks[3]);
}

fn draw_creating_database(f: &mut Frame, name: &str, area: Rect) {
	let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
	let spinner = spinner_chars[0]; // Would be animated in practice

	let msg = format!("{} Creating database '{}'...\n\nPress Esc to cancel", spinner, name);

	let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Creating Database"));
	f.render_widget(paragraph, area);
}

fn draw_database_selection(f: &mut Frame, app: &App, area: Rect) {
	let items: Vec<ListItem> = app.databases.iter().map(|db| ListItem::new(db.as_str())).collect();
	let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Select Database")).highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol(">> ");
	f.render_stateful_widget(list, area, &mut app.list_state.clone());
}

fn draw_subject_selection(f: &mut Frame, app: &App, area: Rect) {
	if app.subjects.is_empty() {
		// New database - show instructions
		let msg = "No subjects in this database yet.\n\n\
                   Press 'n' to create a new subject\n\
                   Press 'Esc' to return to main menu";
		let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Empty Database")).style(Style::default().fg(Color::Cyan));
		f.render_widget(paragraph, area);
	} else {
		// Existing code for showing subjects list
		let items: Vec<ListItem> = app.subjects.iter().map(|s| ListItem::new(s.name())).collect();
		let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Select Subject")).highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol(">> ");
		f.render_stateful_widget(list, area, &mut app.list_state.clone());
	}
}

fn draw_aspect_selection(f: &mut Frame, app: &App, area: Rect) {
	if app.aspects.is_empty() {
		// No aspects - show instructions
		let msg = "No aspects for this subject yet.\n\n\
                   Press 'n' to create a new aspect\n\
                   Press 'Esc' to go back";
		let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Empty Subject")).style(Style::default().fg(Color::Cyan));
		f.render_widget(paragraph, area);
	} else {
		// Existing code for showing aspects list
		let items: Vec<ListItem> = app.aspects.iter().map(|a| ListItem::new(a.name())).collect();
		let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Select Aspect")).highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol(">> ");
		f.render_stateful_widget(list, area, &mut app.list_state.clone());
	}
}

fn draw_loading(f: &mut Frame, loaded_count: usize, total_scanned: usize, status: &str, area: Rect) {
	let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
	// Use frame count approximation based on scanned count for animation
	let spinner_idx = (total_scanned / 100) % spinner_chars.len();
	let spinner = spinner_chars[spinner_idx];

	let percent = if total_scanned > 0 { (loaded_count as f64 / total_scanned as f64 * 100.0) as u32 } else { 0 };
	let progress_bar = if total_scanned > 0 {
		let filled = (percent / 5) as usize; // 20 chars wide
		let empty = 20 - filled;
		format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
	} else {
		"[Loading...]".to_string()
	};

	let msg = format!("{} {}\n\nStatus: {}\n\nPoints loaded: {}\nRecords scanned: {}\nProgress: {}%\n\n{}\n\nPress Esc to cancel", spinner, "Loading measurements...", status, loaded_count, total_scanned, percent, progress_bar);
	let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Loading"));
	f.render_widget(paragraph, area);
}

/// Draw loading screen with compression pane visible
fn draw_loading_with_compression(f: &mut Frame, app: &App, loaded_count: usize, total_scanned: usize, status: &str, area: Rect) {
	// Split horizontally: loading area + compression pane (75/25)
	let h_chunks = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
		.split(area);

	// Draw loading in left area
	draw_loading(f, loaded_count, total_scanned, status, h_chunks[0]);

	// Draw compression pane in right area
	draw_compression_pane(f, app, h_chunks[1]);
}

#[allow(dead_code)] // Reserved for future progress indicator feature
fn draw_plot_with_progress(f: &mut Frame, app: &App, loaded: usize, total: usize, status: &str, area: Rect) {
	// Split area: plot area + progress bar at bottom
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(0),    // Plot area
			Constraint::Length(3), // Progress bar
		])
		.split(area);

	// Draw the plot in the upper area
	draw_plot(f, app, chunks[0]);

	// Draw progress bar at the bottom
	draw_import_progress_bar(f, loaded, total, status, chunks[1]);
}

/// Draw plot with both import progress and compression pane
fn draw_plot_with_progress_and_compression(f: &mut Frame, app: &App, loaded: usize, total: usize, status: &str, area: Rect) {
	// Split horizontally: plot + compression pane (75/25)
	let h_chunks = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
		.split(area);

	// Split the left side vertically: plot + progress bar
	let v_chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(0),    // Plot area
			Constraint::Length(3), // Progress bar
		])
		.split(h_chunks[0]);

	// Draw the plot
	draw_plot(f, app, v_chunks[0]);

	// Draw progress bar
	draw_import_progress_bar(f, loaded, total, status, v_chunks[1]);

	// Draw the compression pane on the right
	draw_compression_pane(f, app, h_chunks[1]);
}

/// Draw the import progress bar
fn draw_import_progress_bar(f: &mut Frame, loaded: usize, total: usize, status: &str, area: Rect) {
	let percent = if total > 0 { (loaded as f64 / total as f64 * 100.0) as u32 } else { 0 };
	let filled = (percent / 5) as usize;
	let empty = 20 - filled;
	let progress_bar = format!("[{}{}] {}%", "█".repeat(filled), "░".repeat(empty), percent);

	let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
	let spinner_idx = (loaded / 1000) % spinner_chars.len();
	let spinner = spinner_chars[spinner_idx];

	let progress_text = if total > 0 { format!("{} {} | {}/{} rows | Esc to cancel", spinner, progress_bar, loaded, total) } else { format!("{} {}", spinner, status) };

	let progress_widget = Paragraph::new(progress_text).block(Block::default().borders(Borders::TOP).title("Import Progress"));
	f.render_widget(progress_widget, area);
}

/// Format time labels for chart axes in a readable, concise way
fn format_time_label(value_seconds: f64, resolution: Resolution) -> String {
	// Convert from seconds to the resolution unit, then format appropriately
	// value_seconds is always the relative time in seconds from the first measurement
	let (display_value, unit_suffix) = match resolution {
		Resolution::Seconds => {
			if value_seconds >= 3600.0 {
				(value_seconds / 3600.0, "h")
			} else if value_seconds >= 60.0 {
				(value_seconds / 60.0, "m")
			} else {
				(value_seconds, "s")
			}
		}
		Resolution::Minutes => {
			let value_minutes = value_seconds / 60.0;
			if value_minutes >= 1440.0 {
				(value_minutes / 1440.0, "d")
			} else if value_minutes >= 60.0 {
				(value_minutes / 60.0, "h")
			} else {
				(value_minutes, "m")
			}
		}
		Resolution::Hours => {
			let value_hours = value_seconds / 3600.0;
			if value_hours >= 168.0 {
				(value_hours / 168.0, "w")
			} else {
				(value_hours, "h")
			}
		}
		Resolution::Days => {
			let value_days = value_seconds / 86400.0;
			if value_days >= 365.0 {
				(value_days / 365.0, "y")
			} else {
				(value_days, "d")
			}
		}
		Resolution::Weeks => {
			let value_weeks = value_seconds / 604800.0;
			if value_weeks >= 52.0 {
				(value_weeks / 52.0, "y")
			} else {
				(value_weeks, "w")
			}
		}
		Resolution::Months => {
			let value_months = value_seconds / 2592000.0; // ~30 days
			if value_months >= 12.0 {
				(value_months / 12.0, "y")
			} else {
				(value_months, "mo")
			}
		}
		Resolution::Years => {
			let value_years = value_seconds / 31536000.0; // ~365 days
			(value_years, "y")
		}
		Resolution::Milliseconds => {
			let value_ms = value_seconds * 1000.0;
			if value_ms >= 1000.0 {
				(value_ms / 1000.0, "s")
			} else {
				(value_ms, "ms")
			}
		}
		Resolution::Microseconds => {
			let value_us = value_seconds * 1_000_000.0;
			if value_us >= 1000.0 {
				(value_us / 1000.0, "ms")
			} else {
				(value_us, "µs")
			}
		}
		Resolution::Nanoseconds => {
			let value_ns = value_seconds * 1_000_000_000.0;
			if value_ns >= 1000.0 {
				(value_ns / 1000.0, "µs")
			} else {
				(value_ns, "ns")
			}
		}
	};

	// Format with appropriate precision
	if display_value >= 100.0 {
		format!("{:.0}{}", display_value, unit_suffix)
	} else if display_value >= 10.0 {
		format!("{:.1}{}", display_value, unit_suffix)
	} else {
		format!("{:.2}{}", display_value, unit_suffix)
	}
}

/// Format a time label with both relative time and absolute date
/// Format: "1y (2024)" - single line since ratatui doesn't render multi-line labels well
fn format_time_label_with_date(value_seconds: f64, resolution: Resolution, first_timestamp: DateTime<Utc>) -> String {
	let relative_label = format_time_label(value_seconds, resolution);

	// Calculate the actual timestamp
	let actual_timestamp = first_timestamp + chrono::Duration::milliseconds((value_seconds * 1000.0) as i64);

	// Format the date portion based on resolution
	let date_label = format_date_for_resolution(actual_timestamp, resolution);

	format!("{} ({})", relative_label, date_label)
}

/// Format a date appropriate for the given resolution
fn format_date_for_resolution(timestamp: DateTime<Utc>, resolution: Resolution) -> String {
	match resolution {
		// For fine resolutions, show time with date
		Resolution::Nanoseconds | Resolution::Microseconds | Resolution::Milliseconds => {
			format!("{:02}:{:02}:{:02}.{:03}",
				timestamp.hour(),
				timestamp.minute(),
				timestamp.second(),
				timestamp.timestamp_subsec_millis())
		}
		Resolution::Seconds => {
			format!("{:02}:{:02}:{:02}",
				timestamp.hour(),
				timestamp.minute(),
				timestamp.second())
		}
		Resolution::Minutes => {
			format!("{:02}/{:02} {:02}:{:02}",
				timestamp.month(),
				timestamp.day(),
				timestamp.hour(),
				timestamp.minute())
		}
		Resolution::Hours => {
			format!("{:02}/{:02} {:02}:00",
				timestamp.month(),
				timestamp.day(),
				timestamp.hour())
		}
		Resolution::Days => {
			format!("{}-{:02}-{:02}",
				timestamp.year(),
				timestamp.month(),
				timestamp.day())
		}
		Resolution::Weeks => {
			format!("{}-{:02}-{:02}",
				timestamp.year(),
				timestamp.month(),
				timestamp.day())
		}
		Resolution::Months => {
			format!("{}-{:02}",
				timestamp.year(),
				timestamp.month())
		}
		Resolution::Years => {
			format!("{}", timestamp.year())
		}
	}
}

fn draw_plot(f: &mut Frame, app: &App, area: Rect) {
	let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(0)]).split(area);

	let info = format!("Aspect: {} | Resolution: {:?} | Points: {} | +/-: adjust resolution | i: Import CSV | q: Quit | Esc: Back", app.selected_aspect.as_ref().unwrap().name(), app.view_resolution, app.measurements.len());
	let paragraph = Paragraph::new(info).block(Block::default().borders(Borders::ALL).title("Info"));
	f.render_widget(paragraph, chunks[0]);

	if app.measurements.is_empty() {
		let msg = Paragraph::new("No measurements to plot").block(Block::default().borders(Borders::ALL).title("Plot"));
		f.render_widget(msg, chunks[1]);
		return;
	}

	// Convert timestamps to relative time (seconds from first measurement)
	let first_timestamp = app.measurements.first().unwrap().0;
	let mut data: Vec<(f64, f64)> = app
		.measurements
		.iter()
		.map(|(t, v)| {
			let relative_time = (t.timestamp_millis() - first_timestamp.timestamp_millis()) as f64 / 1000.0; // Convert to seconds
			(relative_time, *v)
		})
		.collect();

	// compute bounds for axes with small padding
	let x_min = data.first().map(|(x, _)| *x).unwrap_or(0.0);
	let x_max = data.last().map(|(x, _)| *x).unwrap_or(x_min + 1.0);
	let mut y_min = data.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min);
	let mut y_max = data.iter().map(|(_, y)| *y).fold(f64::NEG_INFINITY, f64::max);
	if !(y_min.is_finite() && y_max.is_finite()) {
		y_min = 0.0;
		y_max = 1.0;
	}
	if (x_max - x_min).abs() < f64::EPSILON {
		// ensure non-zero x range
		data.push((x_max + 1.0, data.last().map(|(_, y)| *y).unwrap_or(0.0)));
	}
	if (y_max - y_min).abs() < f64::EPSILON {
		// small padding
		y_min -= 1.0;
		y_max += 1.0;
	} else {
		let y_pad = (y_max - y_min) * 0.1;
		y_min -= y_pad;
		y_max += y_pad;
	}

	let datasets = vec![Dataset::default().name("Measurements").marker(ratatui::symbols::Marker::Braille).graph_type(GraphType::Line).style(Style::default().fg(Color::Cyan)).data(&data)];

	let aspect = app.selected_aspect.as_ref().unwrap();
	let aspect_name = aspect.name().to_string();
	// Use the view resolution for time axis formatting, not native resolution
	let resolution = app.view_resolution;

	// Format time axis label based on resolution
	let time_unit = match resolution {
		Resolution::Nanoseconds => "ns",
		Resolution::Microseconds => "µs",
		Resolution::Milliseconds => "ms",
		Resolution::Seconds => "s",
		Resolution::Minutes => "min",
		Resolution::Hours => "h",
		Resolution::Days => "d",
		Resolution::Weeks => "w",
		Resolution::Months => "mo",
		Resolution::Years => "y",
	};
	let time_axis_title = format!("Time [{}]", time_unit);

	// Generate x-axis labels with min, mid, max values in readable format
	// Include both relative time and actual date
	let x_mid = (x_min + x_max) / 2.0;
	let x_labels = [
		format_time_label_with_date(x_min, resolution, first_timestamp),
		format_time_label_with_date(x_mid, resolution, first_timestamp),
		format_time_label_with_date(x_max, resolution, first_timestamp),
	];

	// Generate y-axis labels with min, mid, max values
	let y_mid = (y_min + y_max) / 2.0;
	let y_labels = [format!("{:.2}", y_min), format!("{:.2}", y_mid), format!("{:.2}", y_max)];

	let chart = Chart::new(datasets).block(Block::default().borders(Borders::ALL).title("Plot")).x_axis(ratatui::widgets::Axis::default().title(time_axis_title).bounds([x_min, x_max]).labels(x_labels)).y_axis(ratatui::widgets::Axis::default().title(aspect_name).bounds([y_min, y_max]).labels(y_labels));

	f.render_widget(chart, chunks[1]);

	// Show streaming indicator when loading is in progress
	if app.loading_in_progress {
		let indicator = Paragraph::new(" Streaming data... ")
			.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
		let chart_area = chunks[1];
		let area = Rect::new(chart_area.x + 2, chart_area.y + chart_area.height.saturating_sub(2), 20, 1);
		f.render_widget(indicator, area);
	}
}

fn draw_subject_name_input(f: &mut Frame, input: &str, cursor: usize, error: Option<&str>, area: Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3), // Title
			Constraint::Length(3), // Input
			Constraint::Length(3), // Error
			Constraint::Min(5),    // Instructions
		])
		.split(area);

	// Title
	let title = Paragraph::new("Create New Subject").block(Block::default().borders(Borders::ALL));
	f.render_widget(title, chunks[0]);

	// Input with cursor
	let display = if cursor < input.len() { format!("{}|{}", &input[..cursor], &input[cursor..]) } else { format!("{}|", input) };
	let input_box = Paragraph::new(display).block(Block::default().borders(Borders::ALL).title("Subject Name"));
	f.render_widget(input_box, chunks[1]);

	// Error
	if let Some(err) = error {
		let error_box = Paragraph::new(err).block(Block::default().borders(Borders::ALL)).style(Style::default().fg(Color::Red));
		f.render_widget(error_box, chunks[2]);
	}

	// Instructions
	let help = "Enter subject name\n\
                Enter: Create | Esc: Back";
	let help_box = Paragraph::new(help).block(Block::default().borders(Borders::ALL).title("Instructions"));
	f.render_widget(help_box, chunks[3]);
}

fn draw_creating_subject(f: &mut Frame, name: &str, area: Rect) {
	let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
	let spinner = spinner_chars[0];

	let msg = format!("{} Creating subject '{}'...\n\nPress Esc to cancel", spinner, name);

	let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Creating Subject"));
	f.render_widget(paragraph, area);
}

fn draw_aspect_name_input(f: &mut Frame, input: &str, cursor: usize, error: Option<&str>, selected_resolution: usize, area: Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3),  // Title
			Constraint::Length(3),  // Input
			Constraint::Length(3),  // Error
			Constraint::Length(14), // Resolution selector (10 resolutions + borders)
			Constraint::Min(3),     // Instructions
		])
		.split(area);

	// Title
	let title = Paragraph::new("Create New Aspect").block(Block::default().borders(Borders::ALL));
	f.render_widget(title, chunks[0]);

	// Input with cursor
	let display = if cursor < input.len() { format!("{}|{}", &input[..cursor], &input[cursor..]) } else { format!("{}|", input) };
	let input_box = Paragraph::new(display).block(Block::default().borders(Borders::ALL).title("Aspect Name"));
	f.render_widget(input_box, chunks[1]);

	// Error
	if let Some(err) = error {
		let error_box = Paragraph::new(err).block(Block::default().borders(Borders::ALL)).style(Style::default().fg(Color::Red));
		f.render_widget(error_box, chunks[2]);
	}

	// Resolution selector
	let resolutions = vec!["Nanoseconds", "Microseconds", "Milliseconds", "Seconds", "Minutes", "Hours", "Days", "Weeks", "Months", "Years"];
	let items: Vec<ListItem> = resolutions.iter().map(|r| ListItem::new(*r)).collect();

	let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Resolution (↑↓ to select)")).highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol(">> ");

	let mut list_state = ratatui::widgets::ListState::default();
	list_state.select(Some(selected_resolution));
	f.render_stateful_widget(list, chunks[3], &mut list_state);

	// Instructions
	let help = "Enter aspect name and select resolution\n\
                ↑↓: Change resolution | Enter: Create | Esc: Back";
	let help_box = Paragraph::new(help).block(Block::default().borders(Borders::ALL).title("Instructions"));
	f.render_widget(help_box, chunks[4]);
}

fn draw_creating_aspect(f: &mut Frame, name: &str, area: Rect) {
	let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
	let spinner = spinner_chars[0];

	let msg = format!("{} Creating aspect '{}'...\n\nPress Esc to cancel", spinner, name);

	let paragraph = Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title("Creating Aspect"));
	f.render_widget(paragraph, area);
}

fn draw_csv_path_input(f: &mut Frame, input: &str, cursor: usize, error: Option<&str>, area: Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3), // Title
			Constraint::Length(3), // Input
			Constraint::Length(3), // Error
			Constraint::Min(5),    // Instructions
		])
		.split(area);

	// Title
	let title = Paragraph::new("Import CSV File").block(Block::default().borders(Borders::ALL));
	f.render_widget(title, chunks[0]);

	// Input with cursor
	let display = if cursor < input.len() { format!("{}|{}", &input[..cursor], &input[cursor..]) } else { format!("{}|", input) };
	let input_box = Paragraph::new(display).block(Block::default().borders(Borders::ALL).title("CSV File Path"));
	f.render_widget(input_box, chunks[1]);

	// Error
	if let Some(err) = error {
		let error_box = Paragraph::new(err).block(Block::default().borders(Borders::ALL)).style(Style::default().fg(Color::Red));
		f.render_widget(error_box, chunks[2]);
	}

	// Instructions
	let help = "Enter path to CSV file (with timestamp and value columns)\n\
                Enter: Import | Esc: Cancel";
	let help_box = Paragraph::new(help).block(Block::default().borders(Borders::ALL).title("Instructions"));
	f.render_widget(help_box, chunks[3]);
}

fn draw_error(f: &mut Frame, msg: &str, area: Rect) {
	let paragraph = Paragraph::new(format!("Error: {}\n\nPress Esc or Enter to return to database selection", msg)).block(Block::default().borders(Borders::ALL).title("Error"));
	f.render_widget(paragraph, area);
}

/// Draw compression mode selection screen
fn draw_compression_mode_selection(f: &mut Frame, app: &App, selected_index: usize, area: Rect) {
	let items: Vec<ListItem> = vec![
		ListItem::new("Disabled - Remove compression config"),
		ListItem::new("Time-based (7 years) - Keep 7 years uncompressed, 1-year tiers"),
		ListItem::new("Time-based (custom) - Configure time-based settings"),
		ListItem::new("Size-based (10 GB) - Compress to fit within 10GB"),
		ListItem::new("Size-based (1 GB) - Compress to fit within 1GB"),
		ListItem::new("Size-based (custom) - Configure size-based settings"),
		ListItem::new("Combined - Both time and size based"),
	];

	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(0),    // List area
			Constraint::Length(3), // Instructions
		])
		.split(area);

	let list = List::new(items)
		.block(Block::default().borders(Borders::ALL).title("Configure Compression"))
		.highlight_style(Style::default().fg(Color::Yellow))
		.highlight_symbol(">> ");

	let mut list_state = app.compression_list_state;
	list_state.select(Some(selected_index));
	f.render_stateful_widget(list, chunks[0], &mut list_state);

	let help = Paragraph::new("Up/Down: Navigate | Enter: Select | Esc: Cancel")
		.block(Block::default().borders(Borders::ALL).title("Instructions"));
	f.render_widget(help, chunks[1]);
}

/// Draw compression configuration screen
#[allow(clippy::too_many_arguments)]
fn draw_compression_config(
	f: &mut Frame,
	mode: &CompressionMode,
	time_pure_days: &str,
	time_tier_days: &str,
	time_max_tiers: &str,
	time_scaling_index: usize,
	size_target_gb: &str,
	size_min_agg: &str,
	size_max_agg: &str,
	base_resolution_index: usize,
	focused_field: &CompressionField,
	validation_error: Option<&str>,
	area: Rect,
) {
	let title = match mode {
		CompressionMode::TimeBased => "Time-Based Compression",
		CompressionMode::SizeBased => "Size-Based Compression",
		CompressionMode::Combined => "Combined Compression",
	};

	// Calculate number of fields to show
	let field_count = match mode {
		CompressionMode::TimeBased => 5,
		CompressionMode::SizeBased => 4,
		CompressionMode::Combined => 8,
	};

	let mut constraints = vec![Constraint::Length(3); field_count]; // Fields
	constraints.push(Constraint::Length(3)); // Error area
	constraints.push(Constraint::Length(3)); // Instructions
	constraints.push(Constraint::Min(0));    // Padding

	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints(constraints)
		.split(area);

	let scaling_options = ["Linear", "Exponential"];
	let resolution_options = ["Nanoseconds", "Microseconds", "Milliseconds", "Seconds", "Minutes", "Hours", "Days", "Weeks", "Months", "Years"];

	let mut chunk_idx = 0;

	// Time-based fields
	if matches!(mode, CompressionMode::TimeBased | CompressionMode::Combined) {
		// Pure duration
		let style = if *focused_field == CompressionField::TimePureDays {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::TimePureDays {
			format!("{}|", time_pure_days)
		} else {
			time_pure_days.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Pure Duration (days)"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;

		// Tier duration
		let style = if *focused_field == CompressionField::TimeTierDays {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::TimeTierDays {
			format!("{}|", time_tier_days)
		} else {
			time_tier_days.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Tier Duration (days)"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;

		// Max tiers
		let style = if *focused_field == CompressionField::TimeMaxTiers {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::TimeMaxTiers {
			format!("{}|", time_max_tiers)
		} else {
			time_max_tiers.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Max Tiers"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;

		// Scaling dropdown
		let style = if *focused_field == CompressionField::TimeScaling {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let scaling_text = format!("< {} >", scaling_options[time_scaling_index]);
		let field = Paragraph::new(scaling_text)
			.block(Block::default().borders(Borders::ALL).title("Scaling"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;
	}

	// Size-based fields
	if matches!(mode, CompressionMode::SizeBased | CompressionMode::Combined) {
		// Target size
		let style = if *focused_field == CompressionField::SizeTargetGb {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::SizeTargetGb {
			format!("{}|", size_target_gb)
		} else {
			size_target_gb.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Target Size (GB)"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;

		// Min aggressiveness
		let style = if *focused_field == CompressionField::SizeMinAgg {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::SizeMinAgg {
			format!("{}|", size_min_agg)
		} else {
			size_min_agg.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Min Aggressiveness (0-1)"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;

		// Max aggressiveness
		let style = if *focused_field == CompressionField::SizeMaxAgg {
			Style::default().fg(Color::Yellow)
		} else {
			Style::default()
		};
		let display = if *focused_field == CompressionField::SizeMaxAgg {
			format!("{}|", size_max_agg)
		} else {
			size_max_agg.to_string()
		};
		let field = Paragraph::new(display)
			.block(Block::default().borders(Borders::ALL).title("Max Aggressiveness (0-1)"))
			.style(style);
		f.render_widget(field, chunks[chunk_idx]);
		chunk_idx += 1;
	}

	// Base resolution (always shown)
	let style = if *focused_field == CompressionField::BaseResolution {
		Style::default().fg(Color::Yellow)
	} else {
		Style::default()
	};
	let resolution_text = format!("< {} >", resolution_options[base_resolution_index]);
	let field = Paragraph::new(resolution_text)
		.block(Block::default().borders(Borders::ALL).title("Base Resolution"))
		.style(style);
	f.render_widget(field, chunks[chunk_idx]);
	chunk_idx += 1;

	// Error area
	if let Some(err) = validation_error {
		let error_box = Paragraph::new(err)
			.block(Block::default().borders(Borders::ALL))
			.style(Style::default().fg(Color::Red));
		f.render_widget(error_box, chunks[chunk_idx]);
	}
	chunk_idx += 1;

	// Instructions
	let help = Paragraph::new("Up/Down: Fields | Left/Right: Options | Enter: Run | Esc: Back")
		.block(Block::default().borders(Borders::ALL).title(title));
	f.render_widget(help, chunks[chunk_idx]);
}

/// Draw plot with compression status pane on the right
fn draw_plot_with_compression_pane(f: &mut Frame, app: &App, area: Rect) {
	// Split horizontally: plot (70%) + compression pane (30%)
	let chunks = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([
			Constraint::Percentage(70),
			Constraint::Percentage(30),
		])
		.split(area);

	// Draw the plot on the left
	draw_plot(f, app, chunks[0]);

	// Draw the compression pane on the right
	draw_compression_pane(f, app, chunks[1]);
}

/// Draw the compression status pane
fn draw_compression_pane(f: &mut Frame, app: &App, area: Rect) {
	let content = match &app.compression_status {
		Some(CompressionStatus::Running { phase, progress_percent, current_tier, total_tiers, aggressiveness, time_range }) => {
			let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
			let spinner_idx = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_millis() as usize / 100 % spinner_chars.len();
			let spinner = spinner_chars[spinner_idx];

			let progress_str = if let Some(pct) = progress_percent {
				format!("{}%", pct)
			} else {
				"...".to_string()
			};

			let tier_info = match (current_tier, total_tiers) {
				(Some(curr), Some(total)) if *total > 0 => format!("  Tier: {} of {}\n", curr, total),
				_ => String::new(),
			};

			let agg_info = match aggressiveness {
				Some(agg) => format!("  Aggressiveness: {:.0}%\n", agg * 100.0),
				None => String::new(),
			};

			let range_info = match time_range {
				Some((start, end)) => {
					let start_str = start.format("%Y-%m-%d").to_string();
					let end_str = end.format("%Y-%m-%d").to_string();
					format!("  Range: {} to {}\n", start_str, end_str)
				}
				None => String::new(),
			};

			format!(
				"  {} Status: Running\n\n  Phase:\n  {}\n\n{}{}{}  {} Processing {}\n\n  [Esc] Cancel",
				spinner, phase, tier_info, agg_info, range_info, spinner, progress_str
			)
		}
		Some(CompressionStatus::Complete {
			original_count,
			compressed_count,
			compression_ratio,
			final_size_bytes,
			time_based_tiers,
			size_based_iterations,
			duration_ms,
		}) => {
			let ratio_pct = compression_ratio * 100.0;
			let size_str = format_bytes(*final_size_bytes);
			let duration_str = format_duration_ms(*duration_ms);

			format!(
				"  {} Complete!\n\n  Original:  {:>10}\n  Compressed:{:>10}\n  Ratio:     {:>9.1}%\n  Final Size:{:>10}\n\n  Time tiers:    {:>5}\n  Size iters:    {:>5}\n  Duration:   {:>8}\n\n  [Esc] Dismiss",
				"✓",
				format_number(*original_count),
				format_number(*compressed_count),
				ratio_pct,
				size_str,
				time_based_tiers,
				size_based_iterations,
				duration_str
			)
		}
		Some(CompressionStatus::Error(msg)) => {
			format!("  {} Error\n\n  {}\n\n\n\n  [Esc] Dismiss", "✗", msg)
		}
		Some(CompressionStatus::Idle { last_compression, dirty_regions_count }) => {
			match last_compression {
				Some(info) => {
					let ratio_pct = info.compression_ratio * 100.0;
					let time_ago = format_time_ago(info.completed_at);
					let dirty_str = if *dirty_regions_count > 0 {
						format!("\n  Dirty regions: {}", dirty_regions_count)
					} else {
						String::new()
					};
					format!(
						"  Last Compression\n  {}\n\n  Original:  {:>10}\n  Compressed:{:>10}\n  Ratio:     {:>9.1}%\n  Tiers:         {:>5}{}\n\n  [c] Run compression",
						time_ago,
						format_number(info.original_count),
						format_number(info.compressed_count),
						ratio_pct,
						info.time_based_tiers,
						dirty_str
					)
				}
				None => {
					"  No compression history\n\n  Press [c] to configure\n  and run compression".to_string()
				}
			}
		}
		None => {
			"  Loading compression\n  statistics...".to_string()
		}
	};

	let style = match &app.compression_status {
		Some(CompressionStatus::Running { .. }) => Style::default().fg(Color::Yellow),
		Some(CompressionStatus::Complete { .. }) => Style::default().fg(Color::Green),
		Some(CompressionStatus::Error(_)) => Style::default().fg(Color::Red),
		Some(CompressionStatus::Idle { dirty_regions_count, .. }) if *dirty_regions_count > 0 => Style::default().fg(Color::Cyan),
		Some(CompressionStatus::Idle { .. }) => Style::default().fg(Color::Gray),
		None => Style::default().fg(Color::DarkGray),
	};

	let paragraph = Paragraph::new(content)
		.block(Block::default().borders(Borders::ALL).title("Compression"))
		.style(style);
	f.render_widget(paragraph, area);
}

/// Format duration in milliseconds to human-readable string
fn format_duration_ms(ms: u64) -> String {
	if ms >= 60_000 {
		format!("{:.1}m", ms as f64 / 60_000.0)
	} else if ms >= 1_000 {
		format!("{:.1}s", ms as f64 / 1_000.0)
	} else {
		format!("{}ms", ms)
	}
}

/// Format a datetime as time ago string
fn format_time_ago(dt: DateTime<Utc>) -> String {
	let now = Utc::now();
	let duration = now.signed_duration_since(dt);

	if duration.num_days() > 365 {
		format!("{:.1}y ago", duration.num_days() as f64 / 365.0)
	} else if duration.num_days() > 30 {
		format!("{}mo ago", duration.num_days() / 30)
	} else if duration.num_days() > 0 {
		format!("{}d ago", duration.num_days())
	} else if duration.num_hours() > 0 {
		format!("{}h ago", duration.num_hours())
	} else if duration.num_minutes() > 0 {
		format!("{}m ago", duration.num_minutes())
	} else {
		"Just now".to_string()
	}
}

/// Format bytes into human-readable string
fn format_bytes(bytes: u64) -> String {
	const KB: u64 = 1024;
	const MB: u64 = KB * 1024;
	const GB: u64 = MB * 1024;
	const TB: u64 = GB * 1024;

	if bytes >= TB {
		format!("{:.1} TB", bytes as f64 / TB as f64)
	} else if bytes >= GB {
		format!("{:.1} GB", bytes as f64 / GB as f64)
	} else if bytes >= MB {
		format!("{:.1} MB", bytes as f64 / MB as f64)
	} else if bytes >= KB {
		format!("{:.1} KB", bytes as f64 / KB as f64)
	} else {
		format!("{} B", bytes)
	}
}

/// Format number with comma separators
fn format_number(n: usize) -> String {
	let s = n.to_string();
	let mut result = String::new();
	for (i, c) in s.chars().rev().enumerate() {
		if i > 0 && i % 3 == 0 {
			result.push(',');
		}
		result.push(c);
	}
	result.chars().rev().collect()
}
