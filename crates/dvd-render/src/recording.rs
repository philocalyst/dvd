//! An append-only terminal byte journal: fixed geometry header, then flushed
//! `u32 length | u64 timestamp | u8 kind | payload` events.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

const MAGIC: &[u8; 8] = b"DVDREC02";
const LIMIT: usize = 64 * 1024 * 1024;
const OUTPUT: u8 = 0;
const INPUT: u8 = 1;
const RESIZE: u8 = 2;
const MARKER: u8 = 3;
const EXIT: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
	pub columns: u16,
	pub rows: u16,
	pub pixel_width: u32,
	pub pixel_height: u32,
}

impl Geometry {
	fn bytes(self) -> [u8; 12] {
		let mut bytes = [0; 12];
		bytes[..2].copy_from_slice(&self.columns.to_le_bytes());
		bytes[2..4].copy_from_slice(&self.rows.to_le_bytes());
		bytes[4..8].copy_from_slice(&self.pixel_width.to_le_bytes());
		bytes[8..].copy_from_slice(&self.pixel_height.to_le_bytes());
		bytes
	}

	fn read(bytes: &mut &[u8]) -> Result<Self> {
		let bytes = take::<12>(bytes)?;
		let columns = u16::from_le_bytes(bytes[..2].try_into().unwrap());
		let rows = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
		let pixel_width = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
		let pixel_height = u32::from_le_bytes(bytes[8..].try_into().unwrap());
		let geometry = Self {
			columns,
			rows,
			pixel_width,
			pixel_height,
		};
		geometry.validate()?;
		Ok(geometry)
	}

	fn validate(self) -> Result<()> {
		ensure!(
			self.columns > 0 && self.rows > 0,
			"recording geometry must have a grid"
		);
		Ok(())
	}
}

/// Labels are accepted for callers' convenience but not persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingHeader {
	pub geometry: Geometry,
	pub terminal: Option<String>,
	pub title: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedEvent {
	pub timestamp_ns: u64,
	pub event: Event,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
	Output(Vec<u8>),
	Input(Vec<u8>),
	Resize(Geometry),
	Marker(String),
	Exit(Option<i32>),
}

pub struct RecordingWriter {
	file: File,
}

impl RecordingWriter {
	pub fn create(path: impl AsRef<Path>, header: RecordingHeader) -> Result<Self> {
		header.geometry.validate()?;
		let mut options = OpenOptions::new();
		options.write(true).create_new(true);
		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt as _;
			options.mode(0o600);
		}
		let mut file = options
			.open(path.as_ref())
			.with_context(|| format!("creating recording {}", path.as_ref().display()))?;
		file.write_all(MAGIC)?;
		file.write_all(&header.geometry.bytes())?;
		file.flush()?;
		Ok(Self { file })
	}

	/// Append and flush one complete event, making it visible to a reader.
	pub fn append(&mut self, event: TimedEvent) -> Result<()> {
		let body = event.bytes()?;
		ensure!(body.len() <= LIMIT, "recording event is too large");
		self.file.write_all(&(body.len() as u32).to_le_bytes())?;
		self.file.write_all(&body)?;
		self.file.flush()?;
		Ok(())
	}

	pub fn sync_data(&self) -> Result<()> {
		self.file.sync_data().context("syncing recording")
	}

	pub fn finish(&mut self) -> Result<()> {
		self.file.sync_data().context("finalising recording")
	}
}

pub struct RecordingReader {
	file: File,
	header: RecordingHeader,
	recovered: bool,
}

impl RecordingReader {
	pub fn open(path: impl AsRef<Path>) -> Result<Self> {
		let mut file = File::open(path.as_ref())
			.with_context(|| format!("opening recording {}", path.as_ref().display()))?;
		let mut magic = [0; 8];
		file.read_exact(&mut magic)
			.context("reading recording header")?;
		ensure!(&magic == MAGIC, "not a dvd recording");
		let mut geometry = [0; 12];
		file.read_exact(&mut geometry)
			.context("reading recording geometry")?;
		let geometry = Geometry::read(&mut geometry.as_slice())?;
		Ok(Self {
			file,
			header: RecordingHeader {
				geometry,
				terminal: None,
				title: None,
			},
			recovered: false,
		})
	}

	pub fn header(&self) -> &RecordingHeader {
		&self.header
	}

	pub fn recovered(&self) -> bool {
		self.recovered
	}

	pub fn next_event(&mut self) -> Result<Option<TimedEvent>> {
		let start = self.file.stream_position()?;
		let mut length = [0; 4];
		match read_prefix(&mut self.file, &mut length)? {
			0 => {
				self.recovered = false;
				return Ok(None);
			}
			4 => {}
			_ => return self.partial(start),
		}
		let length = u32::from_le_bytes(length) as usize;
		ensure!(length <= LIMIT, "recording event is too large");
		let mut body = vec![0; length];
		if read_prefix(&mut self.file, &mut body)? != length {
			return self.partial(start);
		}
		let event = TimedEvent::read(&body)?;
		self.recovered = false;
		Ok(Some(event))
	}

	fn partial(&mut self, start: u64) -> Result<Option<TimedEvent>> {
		self.recovered = true;
		self.file.seek(SeekFrom::Start(start))?;
		Ok(None)
	}
}

impl TimedEvent {
	fn bytes(&self) -> Result<Vec<u8>> {
		let mut bytes = self.timestamp_ns.to_le_bytes().to_vec();
		match &self.event {
			Event::Output(data) => {
				bytes.push(OUTPUT);
				bytes.extend_from_slice(data);
			}
			Event::Input(data) => {
				bytes.push(INPUT);
				bytes.extend_from_slice(data);
			}
			Event::Resize(geometry) => {
				geometry.validate()?;
				bytes.push(RESIZE);
				bytes.extend_from_slice(&geometry.bytes());
			}
			Event::Marker(marker) => {
				bytes.push(MARKER);
				bytes.extend_from_slice(marker.as_bytes());
			}
			Event::Exit(status) => {
				bytes.push(EXIT);
				if let Some(status) = status {
					bytes.extend_from_slice(&status.to_le_bytes());
				}
			}
		}
		Ok(bytes)
	}

	fn read(body: &[u8]) -> Result<Self> {
		let mut body = body;
		let timestamp_ns = u64::from_le_bytes(take(&mut body)?);
		let event = match take::<1>(&mut body)?[0] {
			OUTPUT => Event::Output(std::mem::take(&mut body).to_vec()),
			INPUT => Event::Input(std::mem::take(&mut body).to_vec()),
			RESIZE => Event::Resize(Geometry::read(&mut body)?),
			MARKER => Event::Marker(String::from_utf8(std::mem::take(&mut body).to_vec())?),
			EXIT if body.is_empty() => Event::Exit(None),
			EXIT if body.len() == 4 => Event::Exit(Some(i32::from_le_bytes(take(&mut body)?))),
			EXIT => bail!("invalid recording exit event"),
			_ => bail!("unknown recording event"),
		};
		ensure!(body.is_empty(), "recording event has trailing bytes");
		Ok(Self {
			timestamp_ns,
			event,
		})
	}
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N]> {
	ensure!(bytes.len() >= N, "truncated recording event");
	let mut value = [0; N];
	value.copy_from_slice(&bytes[..N]);
	*bytes = &bytes[N..];
	Ok(value)
}

fn read_prefix(file: &mut File, bytes: &mut [u8]) -> io::Result<usize> {
	let mut read = 0;
	while read < bytes.len() {
		match file.read(&mut bytes[read..]) {
			Ok(0) => return Ok(read),
			Ok(count) => read += count,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(error),
		}
	}
	Ok(read)
}

#[cfg(test)]
mod tests {
	use std::fs::{self, OpenOptions};
	use std::sync::atomic::{AtomicU64, Ordering};

	use super::*;

	static SERIAL: AtomicU64 = AtomicU64::new(0);

	fn geometry() -> Geometry {
		Geometry {
			columns: 80,
			rows: 24,
			pixel_width: 800,
			pixel_height: 0,
		}
	}

	fn path() -> std::path::PathBuf {
		std::env::temp_dir().join(format!(
			"dvd-recording-{}-{}.dvdrec",
			std::process::id(),
			SERIAL.fetch_add(1, Ordering::Relaxed)
		))
	}

	fn writer(path: &Path) -> RecordingWriter {
		RecordingWriter::create(
			path,
			RecordingHeader {
				geometry: geometry(),
				terminal: None,
				title: None,
			},
		)
		.unwrap()
	}

	fn event(time: u64, data: &[u8]) -> TimedEvent {
		TimedEvent {
			timestamp_ns: time,
			event: Event::Output(data.to_vec()),
		}
	}

	#[test]
	fn raw_bytes_timestamps_and_terminal_events_round_trip() {
		let path = path();
		let events = vec![
			TimedEvent {
				timestamp_ns: 0,
				event: Event::Output(vec![0, 0xff, b'\x1b']),
			},
			TimedEvent {
				timestamp_ns: 1,
				event: Event::Input(vec![0xff, 0]),
			},
		];
		let mut writer = writer(&path);
		for event in &events {
			writer.append(event.clone()).unwrap();
		}
		writer.finish().unwrap();
		let mut reader = RecordingReader::open(&path).unwrap();
		assert_eq!(reader.header().geometry, geometry());
		let mut actual = Vec::new();
		while let Some(event) = reader.next_event().unwrap() {
			actual.push(event);
		}
		assert_eq!(actual, events);
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn a_partial_tail_is_ignored_then_retried_when_completed() {
		let source = path();
		let events = [event(1, b"first"), event(2, b"second")];
		let mut writer = writer(&source);
		for event in &events {
			writer.append(event.clone()).unwrap();
		}
		writer.finish().unwrap();
		let bytes = fs::read(&source).unwrap();
		let tail = path();
		let cut = bytes.len() - 2;
		fs::write(&tail, &bytes[..cut]).unwrap();
		let mut reader = RecordingReader::open(&tail).unwrap();
		assert_eq!(reader.next_event().unwrap(), Some(events[0].clone()));
		assert_eq!(reader.next_event().unwrap(), None);
		assert!(reader.recovered());
		OpenOptions::new()
			.append(true)
			.open(&tail)
			.unwrap()
			.write_all(&bytes[cut..])
			.unwrap();
		assert_eq!(reader.next_event().unwrap(), Some(events[1].clone()));
		fs::remove_file(source).unwrap();
		fs::remove_file(tail).unwrap();
	}
}
