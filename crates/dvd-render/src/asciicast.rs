//! Streaming readers for the asciicast v2 and v3 interchange formats.
//!
//! Both versions are newline-delimited JSON, so a [`BufRead`] is enough to
//! replay a file, a pipe, or a live connection without buffering a recording
//! in memory. The reader normalises v2's absolute timestamps and v3's relative
//! intervals into the absolute timeline used by [`crate::source::EventSource`].

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};

use crate::model::Palette;
use crate::source::{
	EventSource, TerminalEvent, TerminalMetadata, TerminalSize, TerminalTheme, TimedTerminalEvent,
};

/// The two asciicast versions understood by this reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsciicastVersion {
	V2,
	V3,
}

/// A streaming asciicast reader.
pub struct AsciicastSource<R> {
	reader: BufReader<R>,
	metadata: TerminalMetadata,
	version: AsciicastVersion,
	last_time: Duration,
	line: usize,
}

impl<R: Read> AsciicastSource<R> {
	/// Read and validate the header, leaving the event stream unread.
	pub fn new(reader: R) -> Result<Self> {
		let mut reader = BufReader::new(reader);
		let mut line = String::new();
		let bytes = reader
			.read_line(&mut line)
			.context("reading asciicast header")?;
		ensure!(bytes != 0, "asciicast is missing its header");
		ensure!(
			!line.trim_start().starts_with('#'),
			"asciicast header must be the first line"
		);

		let header: Value = serde_json::from_str(line.trim_end_matches(['\r', '\n']))
			.context("parsing asciicast header")?;
		let header = header
			.as_object()
			.context("asciicast header must be a JSON object")?;
		let version = match required_u64(header, "version")? {
			2 => AsciicastVersion::V2,
			3 => AsciicastVersion::V3,
			version => bail!("unsupported asciicast version {version}"),
		};
		let term = match version {
			AsciicastVersion::V2 => None,
			AsciicastVersion::V3 => Some(required_object(header, "term")?),
		};
		let size = match (version, term) {
			(AsciicastVersion::V2, _) => TerminalSize::new(
				required_u16(header, "width")?,
				required_u16(header, "height")?,
			),
			(AsciicastVersion::V3, Some(term)) => {
				TerminalSize::new(required_u16(term, "cols")?, required_u16(term, "rows")?)
			}
			(AsciicastVersion::V3, None) => unreachable!("v3 always has a term object"),
		};

		let mut metadata = TerminalMetadata::new(size);
		metadata.timestamp = optional_u64(header, "timestamp")?;
		metadata.idle_time_limit = optional_nonnegative_f64(header, "idle_time_limit")?;
		metadata.command = optional_string(header, "command")?;
		metadata.title = optional_string(header, "title")?;
		metadata.environment = optional_string_map(header, "env")?;
		metadata.tags = optional_string_array(header, "tags")?;
		if let Some(term) = term {
			metadata.terminal_type = optional_string(term, "type")?;
			metadata.terminal_version = optional_string(term, "version")?;
		}
		metadata.theme = optional_theme(term.unwrap_or(header))?;

		let known: &[&str] = match version {
			AsciicastVersion::V2 => &[
				"version",
				"width",
				"height",
				"timestamp",
				"duration",
				"idle_time_limit",
				"command",
				"title",
				"env",
				"theme",
			],
			AsciicastVersion::V3 => &[
				"version",
				"term",
				"timestamp",
				"idle_time_limit",
				"command",
				"title",
				"env",
				"tags",
			],
		};
		metadata.extra = header
			.iter()
			.filter(|(key, _)| !known.contains(&key.as_str()))
			.map(|(key, value)| (key.clone(), value.clone()))
			.collect();

		Ok(Self {
			reader,
			metadata,
			version,
			last_time: Duration::ZERO,
			line: 1,
		})
	}

	pub fn version(&self) -> AsciicastVersion {
		self.version
	}

	fn read_event(&mut self) -> Result<Option<Value>> {
		let mut line = String::new();
		loop {
			line.clear();
			let bytes = self
				.reader
				.read_line(&mut line)
				.with_context(|| format!("reading asciicast line {}", self.line + 1))?;
			if bytes == 0 {
				return Ok(None);
			}
			self.line += 1;
			let line = line.trim_end_matches(['\r', '\n']).trim();
			if line.is_empty() || line.starts_with('#') {
				continue;
			}
			return serde_json::from_str(line)
				.with_context(|| format!("parsing asciicast event on line {}", self.line))
				.map(Some);
		}
	}
}

impl<R: Read> EventSource for AsciicastSource<R> {
	fn metadata(&self) -> &TerminalMetadata {
		&self.metadata
	}

	fn next_event(&mut self) -> Result<Option<TimedTerminalEvent>> {
		let Some(value) = self.read_event()? else {
			return Ok(None);
		};
		let tuple = value
			.as_array()
			.context("asciicast event must be a JSON array")?;
		ensure!(tuple.len() == 3, "asciicast event must have three elements");
		let stamp = tuple[0]
			.as_f64()
			.context("asciicast event time must be a number")?;
		let stamp = duration_from_seconds(stamp).context("invalid asciicast event time")?;
		let time = match self.version {
			AsciicastVersion::V2 => {
				ensure!(
					stamp >= self.last_time,
					"asciicast v2 event times must be ordered"
				);
				stamp
			}
			AsciicastVersion::V3 => self
				.last_time
				.checked_add(stamp)
				.context("asciicast v3 timeline exceeds duration limits")?,
		};
		self.last_time = time;

		let code = tuple[1]
			.as_str()
			.context("asciicast event code must be a string")?;
		let event = match code {
			"o" => TerminalEvent::Output(event_bytes(&tuple[2], "output")?),
			"i" => TerminalEvent::Input(event_bytes(&tuple[2], "input")?),
			"m" => TerminalEvent::Marker(event_string(&tuple[2], "marker")?),
			"r" => TerminalEvent::Resize(parse_size(&tuple[2])?),
			"x" => TerminalEvent::Exit(parse_exit(&tuple[2])?),
			_ => TerminalEvent::Unknown {
				code: code.to_owned(),
				data: tuple[2].clone(),
			},
		};

		Ok(Some(TimedTerminalEvent { time, event }))
	}
}

fn required_object<'a>(
	object: &'a Map<String, Value>,
	key: &str,
) -> Result<&'a Map<String, Value>> {
	object
		.get(key)
		.and_then(Value::as_object)
		.with_context(|| format!("asciicast header field {key:?} must be an object"))
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64> {
	object
		.get(key)
		.and_then(Value::as_u64)
		.with_context(|| format!("asciicast header field {key:?} must be an unsigned integer"))
}

fn required_u16(object: &Map<String, Value>, key: &str) -> Result<u16> {
	let value = required_u64(object, key)?;
	ensure!(
		value != 0 && value <= u16::MAX as u64,
		"asciicast {key} is out of range"
	);
	Ok(value as u16)
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
	object.get(key).map_or(Ok(None), |value| {
		value
			.as_u64()
			.map(Some)
			.with_context(|| format!("asciicast header field {key:?} must be an unsigned integer"))
	})
}

fn optional_nonnegative_f64(object: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
	object.get(key).map_or(Ok(None), |value| {
		let value = value
			.as_f64()
			.with_context(|| format!("asciicast header field {key:?} must be a number"))?;
		ensure!(
			value.is_finite() && value >= 0.0,
			"asciicast {key} must be non-negative"
		);
		Ok(Some(value))
	})
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
	object.get(key).map_or(Ok(None), |value| {
		value
			.as_str()
			.map(str::to_owned)
			.map(Some)
			.with_context(|| format!("asciicast header field {key:?} must be a string"))
	})
}

fn optional_string_map(
	object: &Map<String, Value>,
	key: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
	let Some(value) = object.get(key) else {
		return Ok(Default::default());
	};
	let map = value
		.as_object()
		.with_context(|| format!("asciicast header field {key:?} must be an object"))?;
	map.iter()
		.map(|(key, value)| {
			let value = value
				.as_str()
				.with_context(|| format!("asciicast environment value {key:?} must be a string"))?;
			Ok((key.clone(), value.to_owned()))
		})
		.collect()
}

fn optional_string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
	let Some(value) = object.get(key) else {
		return Ok(Vec::new());
	};
	let values = value
		.as_array()
		.with_context(|| format!("asciicast header field {key:?} must be an array"))?;
	values
		.iter()
		.map(|value| {
			value
				.as_str()
				.map(str::to_owned)
				.with_context(|| format!("asciicast {key} values must be strings"))
		})
		.collect()
}

fn optional_theme(object: &Map<String, Value>) -> Result<Option<TerminalTheme>> {
	let Some(theme) = object.get("theme") else {
		return Ok(None);
	};
	let theme = theme
		.as_object()
		.context("asciicast theme must be an object")?;
	let theme = TerminalTheme {
		foreground: required_string(theme, "fg")?,
		background: required_string(theme, "bg")?,
		palette: required_string(theme, "palette")?,
	};
	Palette::from_terminal_theme(&theme).context("invalid asciicast terminal theme")?;
	Ok(Some(theme))
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String> {
	object
		.get(key)
		.and_then(Value::as_str)
		.map(str::to_owned)
		.with_context(|| format!("asciicast field {key:?} must be a string"))
}

fn event_string(value: &Value, name: &str) -> Result<String> {
	value
		.as_str()
		.map(str::to_owned)
		.with_context(|| format!("asciicast {name} data must be a string"))
}

fn event_bytes(value: &Value, name: &str) -> Result<Vec<u8>> {
	Ok(event_string(value, name)?.into_bytes())
}

fn parse_size(value: &Value) -> Result<TerminalSize> {
	let value = event_string(value, "resize")?;
	let (columns, rows) = value
		.split_once('x')
		.with_context(|| format!("invalid asciicast resize {value:?}"))?;
	let columns = columns
		.parse::<u16>()
		.context("invalid asciicast resize columns")?;
	let rows = rows
		.parse::<u16>()
		.context("invalid asciicast resize rows")?;
	ensure!(columns > 0 && rows > 0, "asciicast resize must be positive");
	Ok(TerminalSize::new(columns, rows))
}

fn parse_exit(value: &Value) -> Result<Option<i32>> {
	if value.is_null() {
		return Ok(None);
	}
	let status = value
		.as_i64()
		.context("asciicast exit status must be an integer or null")?;
	ensure!(
		status >= 0 && status <= i32::MAX as i64,
		"asciicast exit status is out of range"
	);
	Ok(Some(status as i32))
}

fn duration_from_seconds(seconds: f64) -> Result<Duration> {
	ensure!(
		seconds.is_finite() && seconds >= 0.0 && seconds <= Duration::MAX.as_secs_f64(),
		"asciicast time must be finite and non-negative"
	);
	Ok(Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;

	#[test]
	fn v2_uses_absolute_times_and_preserves_unknown_events() {
		let input = r##"{"version":2,"width":80,"height":24,"title":"demo","theme":{"fg":"#ffffff","bg":"#000000","palette":"#000000:#ffffff:#111111:#222222:#333333:#444444:#555555:#666666"}}
[1.25,"o","hello"]
[2.5,"future",{"chapter":1}]
"##;
		let mut source = AsciicastSource::new(Cursor::new(input)).unwrap();
		assert_eq!(source.version(), AsciicastVersion::V2);
		assert_eq!(source.metadata().size, TerminalSize::new(80, 24));
		assert_eq!(source.metadata().title.as_deref(), Some("demo"));
		assert_eq!(
			source.next_event().unwrap(),
			Some(TimedTerminalEvent {
				time: Duration::from_millis(1250),
				event: TerminalEvent::Output(b"hello".to_vec()),
			})
		);
		assert_eq!(
			source.next_event().unwrap(),
			Some(TimedTerminalEvent {
				time: Duration::from_millis(2500),
				event: TerminalEvent::Unknown {
					code: "future".into(),
					data: serde_json::json!({"chapter": 1}),
				},
			})
		);
		assert!(source.next_event().unwrap().is_none());
	}

	#[test]
	fn v3_accumulates_intervals_and_accepts_comments_and_final_line() {
		let input = r##"{"version":3,"term":{"cols":10,"rows":4,"type":"xterm","version":"VTE(7802)","theme":{"fg":"#ffffff","bg":"#000000","palette":"#000000:#111111:#222222:#333333:#444444:#555555:#666666:#777777"}}}
# stream begins
[0.25,"o","a"]
[0.75,"i","b"]
[1.0,"r","20x8"]
[0.0,"x",0]"##;
		let mut source = AsciicastSource::new(Cursor::new(input)).unwrap();
		assert_eq!(source.metadata().terminal_type.as_deref(), Some("xterm"));
		assert_eq!(
			source.metadata().terminal_version.as_deref(),
			Some("VTE(7802)")
		);
		let first = source.next_event().unwrap().unwrap();
		assert_eq!(first.time, Duration::from_millis(250));
		assert_eq!(
			source.next_event().unwrap().unwrap().time,
			Duration::from_secs(1)
		);
		assert_eq!(
			source.next_event().unwrap().unwrap().event,
			TerminalEvent::Resize(TerminalSize::new(20, 8))
		);
		assert_eq!(
			source.next_event().unwrap().unwrap().event,
			TerminalEvent::Exit(Some(0))
		);
	}

	#[test]
	fn invalid_or_backwards_events_are_rejected() {
		let input =
			"{\"version\":2,\"width\":80,\"height\":24}\n[2,\"o\",\"ok\"]\n[1,\"o\",\"late\"]\n";
		let mut source = AsciicastSource::new(Cursor::new(input)).unwrap();
		source.next_event().unwrap();
		assert!(source.next_event().is_err());
	}

	#[test]
	fn malformed_terminal_theme_is_rejected_before_streaming() {
		let input = r##"{"version":3,"term":{"cols":10,"rows":4,"theme":{"fg":"#fff","bg":"#000000","palette":"#000000:#111111:#222222:#333333:#444444:#555555:#666666:#777777"}}}"##;
		assert!(AsciicastSource::new(Cursor::new(input)).is_err());
	}
}
