//! Generic terminal recording inputs.
//!
//! A renderer should not need to know whether bytes came from a PTY, a
//! persisted asciicast, or a live stream. [`EventSource`] is the narrow seam
//! between those concerns: sources expose terminal metadata and a timed,
//! ordered stream of events, while the terminal model and output sinks remain
//! format-agnostic.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

/// The terminal dimensions associated with a recording or resize event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
	pub columns: u16,
	pub rows: u16,
}

impl TerminalSize {
	pub fn new(columns: u16, rows: u16) -> Self {
		Self { columns, rows }
	}
}

/// The terminal's captured colour theme.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTheme {
	pub foreground: String,
	pub background: String,
	pub palette: String,
}

/// Metadata needed to initialise a terminal before replaying its events.
///
/// Unknown header fields are retained in [`Self::extra`]. This keeps the
/// format boundary forward-compatible without making the renderer understand
/// every producer-specific annotation.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalMetadata {
	pub size: TerminalSize,
	/// The complete source duration, including inactivity after the last event.
	pub duration: Option<Duration>,
	pub terminal_type: Option<String>,
	pub terminal_version: Option<String>,
	pub timestamp: Option<u64>,
	/// Maximum delay between source events when replaying this recording.
	pub idle_time_limit: Option<Duration>,
	pub command: Option<String>,
	pub title: Option<String>,
	pub environment: BTreeMap<String, String>,
	pub tags: Vec<String>,
	pub theme: Option<TerminalTheme>,
	pub extra: BTreeMap<String, Value>,
}

impl TerminalMetadata {
	pub fn new(size: TerminalSize) -> Self {
		Self {
			size,
			duration: None,
			terminal_type: None,
			terminal_version: None,
			timestamp: None,
			idle_time_limit: None,
			command: None,
			title: None,
			environment: BTreeMap::new(),
			tags: Vec::new(),
			theme: None,
			extra: BTreeMap::new(),
		}
	}
}

/// A terminal event independent of its recording or transport format.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
	Output(Vec<u8>),
	Input(Vec<u8>),
	Resize(TerminalSize),
	Marker(String),
	Exit(Option<i32>),
	/// An extension event that this renderer does not interpret.
	Unknown {
		code: String,
		data: Value,
	},
}

/// One event at an absolute position on the recording timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedTerminalEvent {
	pub time: Duration,
	pub event: TerminalEvent,
}

/// A source of ordered terminal events.
pub trait EventSource {
	/// Metadata describing the terminal before the first event.
	fn metadata(&self) -> &TerminalMetadata;

	/// Return the next event, blocking when the underlying source is live.
	fn next_event(&mut self) -> Result<Option<TimedTerminalEvent>>;
}
