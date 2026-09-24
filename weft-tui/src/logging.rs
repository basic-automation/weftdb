use std::{
	collections::VecDeque, fmt::Write as FmtWrite, sync::{Arc, Mutex}
};

use tracing_core::{field::Visit, Subscriber};
use tracing_subscriber::Layer;

/// A circular buffer that stores recent log messages
#[derive(Clone)]
pub struct LogBuffer {
	buffer: Arc<Mutex<VecDeque<String>>>,
	max_lines: usize,
}

impl LogBuffer {
	pub fn new(max_lines: usize) -> Self {
		Self { buffer: Arc::new(Mutex::new(VecDeque::with_capacity(max_lines))), max_lines }
	}

	pub fn add_message(&self, message: String) {
		let mut buffer = self.buffer.lock().unwrap();
		buffer.push_back(message);
		while buffer.len() > self.max_lines {
			buffer.pop_front();
		}
	}

	pub fn get_messages(&self) -> Vec<String> {
		self.buffer.lock().unwrap().iter().cloned().collect()
	}

	pub fn clear(&self) {
		self.buffer.lock().unwrap().clear();
	}
}

/// A custom tracing layer that writes logs to the in-app buffer
pub struct LogBufferLayer {
	buffer: LogBuffer,
}

impl LogBufferLayer {
	pub fn new(buffer: LogBuffer) -> Self {
		Self { buffer }
	}
}

impl<S> Layer<S> for LogBufferLayer
where
	S: Subscriber,
{
	fn on_event(&self, event: &tracing_core::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
		let metadata = event.metadata();
		let level = metadata.level();
		let target = metadata.target();

		let mut message = String::new();
		let mut visitor = LogVisitor(&mut message);
		event.record(&mut visitor);

		let formatted = format!("[{}] {}: {}", level, target, message.trim());
		self.buffer.add_message(formatted);
	}
}

struct LogVisitor<'a>(&'a mut String);

impl<'a> Visit for LogVisitor<'a> {
	fn record_f64(&mut self, field: &tracing_core::Field, value: f64) {
		let _ = write!(self.0, "{}={} ", field.name(), value);
	}

	fn record_i64(&mut self, field: &tracing_core::Field, value: i64) {
		let _ = write!(self.0, "{}={} ", field.name(), value);
	}

	fn record_u64(&mut self, field: &tracing_core::Field, value: u64) {
		let _ = write!(self.0, "{}={} ", field.name(), value);
	}

	fn record_bool(&mut self, field: &tracing_core::Field, value: bool) {
		let _ = write!(self.0, "{}={} ", field.name(), value);
	}

	fn record_str(&mut self, field: &tracing_core::Field, value: &str) {
		if field.name() == "message" {
			let _ = write!(self.0, "{} ", value);
		} else {
			let _ = write!(self.0, "{}={} ", field.name(), value);
		}
	}

	fn record_debug(&mut self, _field: &tracing_core::Field, value: &dyn std::fmt::Debug) {
		let _ = write!(self.0, "{:?} ", value);
	}
}
