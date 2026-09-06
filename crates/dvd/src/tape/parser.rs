//! Tokens to [`Commands`].
//!
//! A hand-written recursive-descent parser, kept hand-written on purpose: its
//! value is behaviour a combinator library would quietly change. Errors do not
//! stop the parse — every mistake in a tape is collected and reported together
//! (`dvd check`) — and each error's message and `line:column` are part of that
//! contract. A `winnow`/`chumsky` rewrite would trade those for a framework's
//! own recovery and diagnostics; the reduction here comes from a typed value
//! vocabulary and a table-driven `Set`, not from swapping the engine.

use super::lexer::Lexer;
use super::token::{Token, TokenType};
use crate::cli::Output;
use anyhow::{Error, Result, anyhow};
use regex::Regex;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ParseError {
	pub token: Token,
	pub message: String,
}

impl fmt::Display for ParseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{:2}:{:<2} │ {}",
			self.token.line, self.token.column, self.message
		)
	}
}

impl std::error::Error for ParseError {}

/// One setting from a `Set` line. The payload types are what `burn` consumes
/// directly, so this enum is the boundary between the language and the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
	Shell(String),
	FontSize(u32),
	FontFamily(String),
	Width(u32),
	Height(u32),
	LetterSpacing(f32),
	LineHeight(f32),
	LoopOffset(f32),
	Theme(String),
	Padding(u32),
	Framerate(u32),
	PlaybackSpeed(f32),
	MarginFill(String),
	Margin(u32),
	BorderRadius(u32),
	WindowBar(String),
	WindowBarSize(u32),
	TypingSpeed(Duration),
	WaitTimeout(Duration),
	WaitPattern(String),
	CursorBlink(bool),
}

/// A modifier chord: the keys pressed together (`["Alt", "c"]`) and an optional
/// `@<time>` pacing. Shared by `Ctrl`, `Alt` and `Shift`, whose only difference
/// is the byte encoding `burn` gives them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chord {
	pub keys: Vec<String>,
	pub rate: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum WaitMode {
	#[default]
	Line,
	Screen,
}

impl FromStr for WaitMode {
	type Err = Error;

	fn from_str(input: &str) -> Result<Self> {
		match input.to_lowercase().as_str() {
			"line" => Ok(WaitMode::Line),
			"screen" => Ok(WaitMode::Screen),
			_ => Err(anyhow!(
				"Wait mode '{input}' not recognized. Valid options: line, screen"
			)),
		}
	}
}

/// One executable step of a tape. Struct variants rather than a parallel family
/// of newtype structs: the fields a step carries *are* the step.
#[derive(Debug, Clone)]
pub enum Commands {
	Type {
		rate: Option<Duration>,
		text: String,
	},
	Sleep {
		duration: Option<Duration>,
	},
	Key {
		rate: Option<Duration>,
		key: TokenType,
		repeat_count: u32,
	},
	Ctrl(Chord),
	Alt(Chord),
	Shift(Chord),
	Set(Setting),
	Output {
		path: PathBuf,
		format: Output,
	},
	Require {
		program: String,
	},
	Wait {
		mode: WaitMode,
		pattern: Option<Regex>,
		timeout: Option<Duration>,
	},
	Screenshot {
		path: PathBuf,
	},
	Copy {
		text: String,
	},
	Env {
		variable: String,
		value: String,
	},
	Paste,
	Hide,
	Show,
}

/// Two lifetimes, not one: the borrow of the lexer is shorter than the borrow
/// of the source it reads from. Tying them together (`&'source mut
/// Lexer<'source>`) makes `&mut`'s invariance demand that a local lexer live as
/// long as the source string, which no caller can satisfy from inside a
/// function — [`super::parse`] would not compile.
pub struct Parser<'lexer, 'source> {
	lexer: &'lexer mut Lexer<'source>,
	errors: Vec<ParseError>,
	current_token: Token,
	peek_token: Token,
}

impl<'lexer, 'source> Parser<'lexer, 'source> {
	pub fn new(lexer: &'lexer mut Lexer<'source>) -> Self {
		let mut parser = Parser {
			lexer,
			errors: Vec::new(),
			current_token: Token::default(),
			peek_token: Token::default(),
		};
		// Prime both the current and peek slots.
		parser.next_token();
		parser.next_token();
		parser
	}

	pub fn parse(&mut self) -> Vec<Commands> {
		let mut commands = Vec::new();
		while self.current_token.token_type != TokenType::Eof {
			if self.current_token.token_type == TokenType::Comment {
				self.next_token();
				continue;
			}
			match self.command() {
				Ok(command) => commands.push(command),
				Err(error) => self.record(error),
			}
			self.next_token();
		}
		commands
	}

	pub fn errors(&self) -> &[ParseError] {
		&self.errors
	}

	fn next_token(&mut self) {
		self.current_token = std::mem::replace(&mut self.peek_token, self.lexer.next_token());
	}

	/// Record an error against wherever the cursor currently sits, which is the
	/// token the failing rule was looking at — the anchor the `line:column`
	/// diagnostics are pinned to.
	fn record(&mut self, error: impl fmt::Display) {
		self.errors.push(ParseError {
			token: self.current_token.clone(),
			message: error.to_string(),
		});
	}

	fn command(&mut self) -> Result<Commands> {
		use TokenType as T;
		Ok(match self.current_token.token_type {
			T::Space
			| T::Backspace
			| T::Delete
			| T::Insert
			| T::Enter
			| T::Escape
			| T::Tab
			| T::Home
			| T::End
			| T::Down
			| T::Left
			| T::Right
			| T::Up
			| T::PageUp
			| T::PageDown => self.parse_keypress(),
			T::Set => Commands::Set(self.parse_set()?),
			T::Output => self.parse_output()?,
			T::Sleep => self.parse_sleep()?,
			T::Type => self.parse_type()?,
			T::Ctrl => Commands::Ctrl(self.parse_ctrl()?),
			T::Alt => Commands::Alt(self.parse_modifier("Alt")?),
			T::Shift => Commands::Shift(self.parse_modifier("Shift")?),
			T::Require => self.parse_require()?,
			T::Wait => self.parse_wait()?,
			T::Screenshot => self.parse_screenshot()?,
			T::Copy => self.parse_copy()?,
			T::Env => self.parse_env()?,
			T::Hide => Commands::Hide,
			T::Show => Commands::Show,
			T::Paste => Commands::Paste,
			_ => return Err(anyhow!("Invalid command: {}", self.current_token.literal)),
		})
	}

	// ---- Value readers ---------------------------------------------------
	//
	// Each consumes the value token(s) after the current keyword and leaves the
	// cursor on the last token it read, matching the one-token-of-lookahead
	// rhythm `parse` drives.

	/// Parse the peeked literal into `T` and advance. Propagates the parse error
	/// with `?` rather than unwrapping, so an out-of-range or malformed value is
	/// a rejected tape, not a panic.
	fn take_parsed<T>(&mut self) -> Result<T>
	where
		T: FromStr,
		<T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
	{
		let value = self.peek_token.literal.parse::<T>()?;
		self.next_token();
		Ok(value)
	}

	/// Take the peeked literal verbatim as a string value and advance.
	fn take_string(&mut self) -> String {
		let literal = self.peek_token.literal.clone();
		self.next_token();
		literal
	}

	/// An optional leading `@<time>`, present on every command that can be paced.
	fn parse_optional_speed(&mut self) -> Option<Duration> {
		(self.peek_token.token_type == TokenType::At).then(|| {
			self.next_token();
			self.parse_time()
		})
	}

	/// An optional trailing repeat count for a keypress; a value too large for
	/// `u32` silently falls back to one, unlike a setting's `?`-propagated value.
	fn parse_repeat(&mut self) -> u32 {
		if self.peek_token.token_type == TokenType::Number {
			let count = self.peek_token.literal.parse().unwrap_or(1);
			self.next_token();
			count
		} else {
			1
		}
	}

	/// A number and an optional unit into a [`Duration`]. The lexer's
	/// `read_number` accepts any run of digits and dots, so it can hand back
	/// something no `f64` can parse (`0.0.0.0`); that records an error and
	/// yields a zero duration rather than panicking. Fractions truncate toward
	/// zero (`1.9s` is one second) because the cast to `u64` is deliberate.
	fn parse_time(&mut self) -> Duration {
		let seconds = if self.peek_token.token_type == TokenType::Number {
			let literal = self.peek_token.literal.clone();
			self.next_token();
			match literal.parse::<f64>() {
				Ok(value) => value,
				Err(_) => {
					self.record(format!("{literal:?} is not a valid time value"));
					return Duration::default();
				}
			}
		} else {
			self.record(format!(
				"Expected time after {}",
				self.current_token.literal
			));
			return Duration::default();
		};
		self.take_unit(seconds, true)
	}

	/// Apply the optional trailing time unit to a bare number of `seconds`,
	/// consuming the unit token when present. `Minutes` is only a unit where the
	/// grammar allows it (`Wait`/`WaitTimeout`, not `TypingSpeed`).
	fn take_unit(&mut self, value: f64, allow_minutes: bool) -> Duration {
		let duration = match self.peek_token.token_type {
			TokenType::Milliseconds => Duration::from_millis(value as u64),
			TokenType::Seconds => Duration::from_secs(value as u64),
			TokenType::Minutes if allow_minutes => Duration::from_secs((value * 60.0) as u64),
			_ => return Duration::from_secs(value as u64),
		};
		self.next_token();
		duration
	}

	// ---- Per-command rules ----------------------------------------------

	fn parse_keypress(&mut self) -> Commands {
		let key = self.current_token.token_type.clone();
		Commands::Key {
			rate: self.parse_optional_speed(),
			repeat_count: self.parse_repeat(),
			key,
		}
	}

	fn parse_type(&mut self) -> Result<Commands> {
		let rate = self.parse_optional_speed();
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}
		Ok(Commands::Type {
			rate,
			text: self.take_concatenated_strings(),
		})
	}

	fn parse_copy(&mut self) -> Result<Commands> {
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}
		Ok(Commands::Copy {
			text: self.take_concatenated_strings(),
		})
	}

	/// Adjacent string literals concatenate, so `Type "cd " "/tmp"` types one
	/// line. Shared by `Type` and `Copy` so the two cannot drift apart.
	fn take_concatenated_strings(&mut self) -> String {
		let mut text = String::new();
		while self.peek_token.token_type == TokenType::String {
			self.next_token();
			text.push_str(&self.current_token.literal);
		}
		text
	}

	fn parse_sleep(&mut self) -> Result<Commands> {
		let duration = (self.peek_token.token_type == TokenType::Number)
			.then(|| self.parse_time())
			.filter(|duration| *duration != Duration::default());
		Ok(Commands::Sleep { duration })
	}

	fn parse_require(&mut self) -> Result<Commands> {
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects one string", self.current_token.literal));
		}
		Ok(Commands::Require {
			program: self.take_string(),
		})
	}

	fn parse_env(&mut self) -> Result<Commands> {
		// A keyword-shaped name arrives as its keyword token, not a `String`, so
		// `Env Set "x"` is rejected here rather than exporting a variable "Set".
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!(
				"Env expects a variable name, got {}",
				self.peek_token.literal
			));
		}
		let variable = self.take_string();
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}
		Ok(Commands::Env {
			variable,
			value: self.take_string(),
		})
	}

	fn parse_screenshot(&mut self) -> Result<Commands> {
		let path = self.take_path(
			|extension| extension == "png",
			"Expected file with .png extension",
		)?;
		Ok(Commands::Screenshot { path })
	}

	fn parse_output(&mut self) -> Result<Commands> {
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("Expected file path after output"));
		}
		let literal = self.peek_token.literal.clone();
		let format = match Path::new(&literal).extension() {
			// Rejected here, at parse time, rather than left for `burn` to
			// discover after the PTY is open — `dvd check` should catch
			// `Output out.bmp` as surely as a missing quote.
			Some(extension) => extension
				.to_string_lossy()
				.parse::<Output>()
				.map_err(|message| anyhow!(message))?,
			// A trailing slash names a folder of PNG stills; anything else with
			// no extension has no format to infer.
			None if literal.ends_with('/') => Output::Png,
			None => return Err(anyhow!("Expected folder with trailing slash")),
		};
		self.next_token();
		Ok(Commands::Output {
			path: PathBuf::from(literal),
			format,
		})
	}

	/// Read a path value, requiring an extension `accept` approves. Advances past
	/// the (attempted) path token in every case so the parse can continue.
	fn take_path(&mut self, accept: impl Fn(&str) -> bool, rejection: &str) -> Result<PathBuf> {
		if self.peek_token.token_type != TokenType::String {
			self.next_token();
			return Err(anyhow!("Expected path after Screenshot"));
		}
		let accepted = Path::new(&self.peek_token.literal)
			.extension()
			.is_some_and(|extension| accept(&extension.to_string_lossy()));
		if !accepted {
			self.next_token();
			return Err(anyhow!("{rejection}"));
		}
		Ok(PathBuf::from(self.take_string()))
	}

	fn parse_wait(&mut self) -> Result<Commands> {
		let mode = if self.peek_token.token_type == TokenType::Plus {
			self.next_token();
			if !matches!(self.peek_token.literal.as_str(), "Line" | "Screen") {
				return Err(anyhow!("Wait+ expects Line or Screen"));
			}
			let mode = self.peek_token.literal.parse()?;
			self.next_token();
			mode
		} else {
			WaitMode::Line
		};

		let timeout = self.parse_optional_speed();

		let pattern = (self.peek_token.token_type == TokenType::Regex)
			.then(|| {
				self.next_token();
				Regex::new(&self.current_token.literal).map_err(|_| {
					anyhow!(
						"Invalid regular expression '{}': invalid regex",
						self.current_token.literal
					)
				})
			})
			.transpose()?;

		Ok(Commands::Wait {
			mode,
			pattern,
			timeout,
		})
	}

	fn parse_ctrl(&mut self) -> Result<Chord> {
		let rate = self.parse_optional_speed();
		let mut keys = Vec::new();
		let mut modifiers_allowed = true;

		while self.peek_token.token_type == TokenType::Plus {
			self.next_token();
			let key = &self.peek_token;

			if key.token_type.is_modifier() {
				if !modifiers_allowed {
					return Err(anyhow!("Modifiers must come before other keys"));
				}
				keys.push(key.literal.clone());
				self.next_token();
				continue;
			}

			modifiers_allowed = false;
			use TokenType as T;
			let is_key = matches!(
				key.token_type,
				T::Enter
					| T::Space | T::Backspace
					| T::Delete | T::Insert
					| T::Tab | T::Escape
					| T::Minus | T::At
					| T::LeftBracket
					| T::RightBracket
					| T::Caret | T::Backslash
			) || (key.token_type == T::String && key.literal.chars().count() == 1);
			if !is_key {
				return Err(anyhow!("Invalid Ctrl key: {}", key.literal));
			}
			keys.push(key.literal.clone());
			self.next_token();
		}

		if keys.is_empty() {
			return Err(anyhow!("Expected at least one key after Ctrl"));
		}
		Ok(Chord { keys, rate })
	}

	/// `Alt` and `Shift` share one grammar: an optional rate, then exactly one
	/// `+<key>` drawn from a small set. The `name` only shapes the error text.
	fn parse_modifier(&mut self, name: &str) -> Result<Chord> {
		let rate = self.parse_optional_speed();
		if self.peek_token.token_type != TokenType::Plus {
			return Err(anyhow!(
				"Expected '+' after {name}, got {}",
				self.peek_token.literal
			));
		}
		self.next_token();

		use TokenType as T;
		let key = &self.peek_token;
		if !matches!(
			key.token_type,
			T::String | T::Enter | T::LeftBracket | T::RightBracket | T::Tab
		) {
			return Err(anyhow!("Invalid {name} key: {}", key.literal));
		}
		Ok(Chord {
			keys: vec![self.take_string()],
			rate,
		})
	}

	fn parse_set(&mut self) -> Result<Setting> {
		if !self.peek_token.token_type.is_setting() {
			return Err(anyhow!("Unknown setting: {}", self.peek_token.literal));
		}
		let setting = self.peek_token.token_type.clone();
		self.next_token();

		use TokenType as T;
		Ok(match setting {
			T::FontSize => Setting::FontSize(self.take_parsed()?),
			T::Width => Setting::Width(self.take_parsed()?),
			T::Height => Setting::Height(self.take_parsed()?),
			T::Padding => Setting::Padding(self.take_parsed()?),
			T::Framerate => Setting::Framerate(self.take_parsed()?),
			T::Margin => Setting::Margin(self.take_parsed()?),
			T::BorderRadius => Setting::BorderRadius(self.take_parsed()?),
			T::WindowBarSize => Setting::WindowBarSize(self.take_parsed()?),
			T::LetterSpacing => Setting::LetterSpacing(self.take_parsed()?),
			T::LineHeight => Setting::LineHeight(self.take_parsed()?),
			T::PlaybackSpeed => Setting::PlaybackSpeed(self.take_parsed()?),
			T::FontFamily => Setting::FontFamily(self.take_string()),
			T::Theme => Setting::Theme(self.take_string()),
			T::MarginFill => Setting::MarginFill(self.take_string()),
			T::WindowBar => Setting::WindowBar(self.take_string()),
			T::Shell => Setting::Shell(self.take_shell()?),
			T::LoopOffset => Setting::LoopOffset(self.take_loop_offset()?),
			T::TypingSpeed => {
				Setting::TypingSpeed(self.take_setting_duration("TypingSpeed", false)?)
			}
			T::WaitTimeout => {
				Setting::WaitTimeout(self.take_setting_duration("WaitTimeout", true)?)
			}
			T::WaitPattern => Setting::WaitPattern(self.take_wait_pattern()?),
			T::CursorBlink => Setting::CursorBlink(self.take_bool()?),
			_ => unreachable!("is_setting admits no other token"),
		})
	}

	fn take_shell(&mut self) -> Result<String> {
		if !matches!(
			self.peek_token.token_type,
			TokenType::String | TokenType::Json
		) {
			return Err(anyhow!(
				"Set Shell expects string or JSON, got {}",
				self.peek_token.literal
			));
		}
		Ok(self.take_string())
	}

	/// A percentage whose `%` is an optional, separate token (`25`, `25 %`,
	/// `25%` all mean 25) — the lexer never fuses the sign onto the number.
	fn take_loop_offset(&mut self) -> Result<f32> {
		let value = self.take_parsed()?;
		if self.peek_token.token_type == TokenType::Percent {
			self.next_token();
		}
		Ok(value)
	}

	fn take_setting_duration(&mut self, name: &str, allow_minutes: bool) -> Result<Duration> {
		if self.peek_token.token_type != TokenType::Number {
			return Err(anyhow!(
				"Set {name} expects a number, got {}",
				self.peek_token.literal
			));
		}
		let value: f64 = self.take_parsed()?;
		Ok(self.take_unit(value, allow_minutes))
	}

	/// `WaitPattern`'s value is a plain `String`, not the `/pattern/` `Regex`
	/// token `Wait` uses, but it must still reach the same validity check.
	fn take_wait_pattern(&mut self) -> Result<String> {
		let pattern = self.peek_token.literal.clone();
		if Regex::new(&pattern).is_err() {
			return Err(anyhow!("Invalid regexp pattern: {pattern}"));
		}
		self.next_token();
		Ok(pattern)
	}

	/// Case-sensitive by spelling, not by token: `True` lexes as a plain string
	/// and fails here just as a misspelling would.
	fn take_bool(&mut self) -> Result<bool> {
		let value = match self.peek_token.literal.as_str() {
			"true" => true,
			"false" => false,
			other => return Err(anyhow!("Set CursorBlink expects true/false, got {other}")),
		};
		self.next_token();
		Ok(value)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tape;
	use rstest::rstest;

	/// Parse a tape expected to be valid, panicking with the errors if it was
	/// not — most tests assert the shape of the commands, not that it parses.
	fn parse_ok(source: &str) -> Vec<Commands> {
		let (commands, errors) = tape::parse(source);
		assert!(
			errors.is_empty(),
			"unexpected parse errors for {source:?}: {errors:?}"
		);
		commands
	}

	/// Parse a tape expected to yield exactly one command, and return it.
	fn one(source: &str) -> Commands {
		let mut commands = parse_ok(source);
		assert_eq!(
			commands.len(),
			1,
			"expected one command from {source:?}, got {commands:?}"
		);
		commands.remove(0)
	}

	/// Assert a tape is rejected and smuggles out no command at all — a broken
	/// line must never leave a half-built command behind.
	fn rejected(source: &str) {
		let (commands, errors) = tape::parse(source);
		assert!(!errors.is_empty(), "expected a parse error for {source:?}");
		assert!(
			commands.is_empty(),
			"no command should survive {source:?}: {commands:?}"
		);
	}

	// ---- Keys and the @<time> rate prefix --------------------------------

	#[rstest]
	#[case("Enter", TokenType::Enter)]
	#[case("Tab", TokenType::Tab)]
	#[case("Space", TokenType::Space)]
	#[case("Backspace", TokenType::Backspace)]
	#[case("Escape", TokenType::Escape)]
	#[case("Delete", TokenType::Delete)]
	#[case("Insert", TokenType::Insert)]
	#[case("Up", TokenType::Up)]
	#[case("Down", TokenType::Down)]
	#[case("Left", TokenType::Left)]
	#[case("Right", TokenType::Right)]
	#[case("PageUp", TokenType::PageUp)]
	#[case("PageDown", TokenType::PageDown)]
	#[case("Home", TokenType::Home)]
	#[case("End", TokenType::End)]
	fn every_named_key_reaches_its_token(#[case] source: &str, #[case] expected: TokenType) {
		let Commands::Key {
			key,
			repeat_count,
			rate,
		} = one(source)
		else {
			panic!("{source} did not parse as a Key command");
		};
		assert_eq!(key, expected);
		assert_eq!(repeat_count, 1);
		assert_eq!(rate, None);
	}

	#[test]
	fn a_keypress_takes_a_repeat_count_after_its_optional_rate() {
		assert!(matches!(
			one("Up 5"),
			Commands::Key {
				repeat_count: 5,
				rate: None,
				..
			}
		));
		assert!(matches!(
			one("Up@20ms 5"),
			Commands::Key { repeat_count: 5, rate: Some(r), .. } if r == Duration::from_millis(20)
		));
	}

	/// The rate prefix sets a duration on every command that accepts one, and on
	/// `Wait` it lands in `timeout` instead — a ceiling, not a keystroke pace.
	#[test]
	fn the_rate_prefix_sets_a_duration_where_each_command_keeps_it() {
		let ms10 = Some(Duration::from_millis(10));
		assert!(matches!(one(r#"Type@10ms "x""#), Commands::Type { rate, .. } if rate == ms10));
		assert!(matches!(one("Ctrl@10ms+c"), Commands::Ctrl(c) if c.rate == ms10));
		assert!(matches!(one("Alt@10ms+a"), Commands::Alt(c) if c.rate == ms10));
		assert!(matches!(one("Shift@10ms+a"), Commands::Shift(c) if c.rate == ms10));
		assert!(matches!(one("Up@10ms"), Commands::Key { rate, .. } if rate == ms10));
		assert!(
			matches!(one("Wait@250ms"), Commands::Wait { timeout, .. } if timeout == Some(Duration::from_millis(250)))
		);
	}

	// ---- Ctrl / Alt / Shift modifier chains ------------------------------

	#[rstest]
	#[case("Ctrl+Alt+c", &["Alt", "c"])]
	#[case("Ctrl+Shift+a", &["Shift", "a"])]
	fn ctrl_chains_accept_alt_and_shift_as_leading_modifiers(
		#[case] source: &str,
		#[case] keys: &[&str],
	) {
		let Commands::Ctrl(chord) = one(source) else {
			panic!("expected a Ctrl command")
		};
		assert_eq!(chord.keys, keys);
	}

	#[test]
	fn alt_and_shift_take_exactly_one_key_after_a_plus() {
		assert!(matches!(one("Alt+Enter"), Commands::Alt(c) if c.keys == ["Enter"]));
		assert!(matches!(one("Shift+Tab"), Commands::Shift(c) if c.keys == ["Tab"]));
	}

	/// Once a non-modifier key appears the chain is closed; a modifier after it
	/// is refused rather than silently reordered. Pinned arbitrarily deep so the
	/// rule stays iterative and cannot overflow the stack.
	#[rstest]
	#[case("Ctrl+c+Alt")]
	#[case("Alt a")]
	fn a_closed_or_malformed_modifier_chain_is_rejected(#[case] source: &str) {
		rejected(source);
	}

	#[test]
	fn a_very_deep_valid_modifier_chain_does_not_overflow_the_stack() {
		let source = format!("Ctrl{}+c", "+Alt".repeat(5_000));
		assert_eq!(parse_ok(&source).len(), 1);
	}

	// ---- Wait ------------------------------------------------------------

	#[rstest]
	#[case("Wait", WaitMode::Line)]
	#[case("Wait+Line", WaitMode::Line)]
	#[case("Wait+Screen", WaitMode::Screen)]
	fn wait_plus_selects_line_or_screen_mode(#[case] source: &str, #[case] expected: WaitMode) {
		let Commands::Wait { mode, .. } = one(source) else {
			panic!("expected a Wait command")
		};
		assert_eq!(mode, expected);
	}

	#[test]
	fn wait_combines_mode_timeout_and_pattern_in_one_command() {
		let Commands::Wait {
			mode,
			timeout,
			pattern,
		} = one("Wait+Screen@2s /ready/")
		else {
			panic!("expected a Wait command");
		};
		assert_eq!(mode, WaitMode::Screen);
		assert_eq!(timeout, Some(Duration::from_secs(2)));
		assert!(pattern.expect("a pattern").is_match("ready"));
	}

	/// A space before the pattern is required: `read_identifier` treats `/` as an
	/// ordinary identifier character, so `Wait/ready/` lexes as one bare word and
	/// never reaches a `Wait` keyword at all. Pinned as a known bug — the fix
	/// would make `/` context-sensitive in a lexer whose design is not that.
	#[test]
	fn a_wait_regex_needs_a_space_or_it_is_swallowed_into_the_keyword() {
		assert!(matches!(
			one("Wait /ready/"),
			Commands::Wait {
				pattern: Some(_),
				..
			}
		));
		rejected("Wait/ready/");
	}

	// ---- Sleep and time literals -----------------------------------------

	#[rstest]
	#[case("Sleep", None)]
	#[case("Sleep 500ms", Some(Duration::from_millis(500)))]
	#[case("Sleep 2s", Some(Duration::from_secs(2)))]
	#[case("Sleep 1m", Some(Duration::from_secs(60)))]
	#[case("Sleep 3", Some(Duration::from_secs(3)))] // no unit defaults to seconds
	#[case("Sleep 1.9s", Some(Duration::from_secs(1)))] // fractions truncate toward zero
	fn sleep_reads_every_time_unit(#[case] source: &str, #[case] expected: Option<Duration>) {
		let Commands::Sleep { duration } = one(source) else {
			panic!("expected a Sleep command")
		};
		assert_eq!(duration, expected);
	}

	// ---- Type / Copy string handling -------------------------------------

	/// Adjacent string literals concatenate, and `Copy` shares the exact rule so
	/// the two cannot drift apart.
	#[rstest]
	#[case(r#"Type "hi""#, "hi")]
	#[case(r#"Type "cd " "/tmp""#, "cd /tmp")]
	fn type_concatenates_adjacent_strings(#[case] source: &str, #[case] text: &str) {
		let Commands::Type { text: parsed, .. } = one(source) else {
			panic!("expected a Type command")
		};
		assert_eq!(parsed, text);
	}

	#[test]
	fn copy_concatenates_adjacent_strings_and_round_trips_with_paste() {
		assert!(matches!(one(r#"Copy "a" "b""#), Commands::Copy { text } if text == "ab"));
		assert!(matches!(one("Paste"), Commands::Paste));
	}

	// ---- Set / Setting ---------------------------------------------------

	#[rstest]
	#[case(r#"Set Shell "zsh""#, Setting::Shell("zsh".into()))]
	#[case(r#"Set Shell {"cmd":"zsh"}"#, Setting::Shell(r#"{"cmd":"zsh"}"#.into()))]
	#[case("Set FontSize 24", Setting::FontSize(24))]
	#[case(r#"Set FontFamily "Fira Code""#, Setting::FontFamily("Fira Code".into()))]
	#[case("Set Width 1200", Setting::Width(1200))]
	#[case("Set Height 600", Setting::Height(600))]
	#[case("Set LetterSpacing 0.5", Setting::LetterSpacing(0.5))]
	#[case("Set LineHeight 1.2", Setting::LineHeight(1.2))]
	#[case("Set LoopOffset 25%", Setting::LoopOffset(25.0))]
	#[case("Set LoopOffset 10", Setting::LoopOffset(10.0))] // the '%' suffix is optional
	#[case("Set LoopOffset 25 %", Setting::LoopOffset(25.0))] // and may be a separate token
	#[case(r#"Set Theme "nord""#, Setting::Theme("nord".into()))]
	#[case("Set Padding 20", Setting::Padding(20))]
	#[case("Set Framerate 60", Setting::Framerate(60))]
	#[case("Set PlaybackSpeed 1.5", Setting::PlaybackSpeed(1.5))]
	#[case(r#"Set MarginFill "black""#, Setting::MarginFill("black".into()))]
	#[case("Set Margin 10", Setting::Margin(10))]
	#[case("Set BorderRadius 8", Setting::BorderRadius(8))]
	#[case(r#"Set WindowBar "Colorful""#, Setting::WindowBar("Colorful".into()))]
	#[case("Set WindowBarSize 40", Setting::WindowBarSize(40))]
	#[case("Set TypingSpeed 2", Setting::TypingSpeed(Duration::from_secs(2)))]
	#[case(
		"Set TypingSpeed 50ms",
		Setting::TypingSpeed(Duration::from_millis(50))
	)]
	#[case("Set WaitTimeout 5", Setting::WaitTimeout(Duration::from_secs(5)))]
	#[case(
		"Set WaitTimeout 500ms",
		Setting::WaitTimeout(Duration::from_millis(500))
	)]
	#[case("Set WaitTimeout 2m", Setting::WaitTimeout(Duration::from_secs(120)))]
	#[case(r#"Set WaitPattern "ready""#, Setting::WaitPattern("ready".into()))]
	#[case("Set CursorBlink true", Setting::CursorBlink(true))]
	#[case("Set CursorBlink false", Setting::CursorBlink(false))]
	fn every_setting_parses_its_declared_value(#[case] source: &str, #[case] expected: Setting) {
		let Commands::Set(setting) = one(source) else {
			panic!("expected a Set command")
		};
		assert_eq!(setting, expected);
	}

	/// Rejections that must each fail loudly rather than build a nonsense `Set`:
	/// an unknown setting name, a case-wrong boolean (`True` lexes as a string),
	/// and a value that is not a valid regex.
	#[rstest]
	#[case("Set Nonsense 1")]
	#[case("Set CursorBlink True")]
	#[case(r#"Set WaitPattern "(""#)]
	#[case("Set FontSize 99999999999")] // overflows u32 — rejected, not panicked
	fn set_rejects_malformed_values(#[case] source: &str) {
		rejected(source);
	}

	// ---- Output / the typed format ---------------------------------------

	#[rstest]
	#[case("Output out.mp4", Output::Mp4, "out.mp4")]
	#[case("Output OUT.MP4", Output::Mp4, "OUT.MP4")] // extension match is case-insensitive
	#[case("Output frame.png", Output::Png, "frame.png")]
	#[case("Output animation.gif", Output::Gif, "animation.gif")]
	#[case("Output movie.svg", Output::Svg, "movie.svg")]
	#[case("Output stills/", Output::Png, "stills/")] // a trailing slash is a folder of PNG stills
	fn output_infers_its_format_from_the_extension(
		#[case] source: &str,
		#[case] format: Output,
		#[case] path: &str,
	) {
		let Commands::Output {
			format: parsed,
			path: parsed_path,
		} = one(source)
		else {
			panic!("expected an Output command");
		};
		assert_eq!(parsed, format);
		assert_eq!(parsed_path, PathBuf::from(path));
	}

	#[test]
	fn a_path_with_no_extension_and_no_trailing_slash_is_rejected() {
		rejected("Output stills");
	}

	/// Rejected at parse time so `dvd check` catches it, and the error names the
	/// allowed extensions rather than failing later once the PTY is open.
	#[test]
	fn an_unsupported_output_extension_is_rejected_naming_the_alternatives() {
		let (commands, errors) = tape::parse("Output out.bmp");
		assert!(
			errors[0].message.contains("mp4"),
			"error should name allowed extensions: {errors:?}"
		);
		assert!(
			!commands
				.iter()
				.any(|c| matches!(c, Commands::Output { .. }))
		);
	}

	// ---- Require / Screenshot / Env / Hide / Show ------------------------

	#[test]
	fn require_captures_the_program_name() {
		assert!(
			matches!(one(r#"Require "ffmpeg""#), Commands::Require { program } if program == "ffmpeg")
		);
	}

	#[test]
	fn screenshot_requires_a_dot_png_path() {
		assert!(
			matches!(one("Screenshot frame.png"), Commands::Screenshot { path } if path.as_path() == Path::new("frame.png"))
		);
		rejected("Screenshot frame.jpg");
	}

	#[test]
	fn env_captures_a_name_and_a_value() {
		let Commands::Env { variable, value } = one(r#"Env API_KEY "secret""#) else {
			panic!("expected an Env command");
		};
		assert_eq!((variable.as_str(), value.as_str()), ("API_KEY", "secret"));
	}

	/// A keyword may not stand in for a variable name — `Env Set "x"` must fail
	/// rather than export a variable called "Set".
	#[test]
	fn env_rejects_a_keyword_as_a_variable_name() {
		rejected(r#"Env Set "value""#);
	}

	#[test]
	fn hide_and_show_carry_no_data() {
		assert!(matches!(one("Hide"), Commands::Hide));
		assert!(matches!(one("Show"), Commands::Show));
	}

	// ---- Comments, layout and error recovery -----------------------------

	#[rstest]
	#[case("# just a note\nSleep 1s", 1)]
	#[case("Sleep 1s\n\n\nType \"hi\"", 2)] // blank lines are ignored
	#[case("Sleep 1s # take a breath\nType \"hi\"", 2)] // a trailing comment too
	fn comments_and_blank_lines_do_not_disturb_the_command_count(
		#[case] source: &str,
		#[case] count: usize,
	) {
		assert_eq!(parse_ok(source).len(), count);
	}

	// ---- Error location --------------------------------------------------

	/// The lexer's `read_number` can hand back a `Number` no `f64` can parse
	/// (`0.0.0.0`), reachable both directly (`Sleep`) and through the `@<time>`
	/// prefix. Both must report an error rather than panic. Column tracking
	/// resets per line, so a mistake on line three is reported there and nowhere
	/// shifted by the lines before it.
	#[rstest]
	#[case("Set FontSize", 1, 5)]
	#[case("Wait /(/", 1, 6)]
	#[case("Type \"ok\"\nSleep 1s\nSleep 0.0.0.0", 3, 7)]
	fn an_error_is_reported_at_the_offending_token(
		#[case] source: &str,
		#[case] line: usize,
		#[case] column: usize,
	) {
		let (_, errors) = tape::parse(source);
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert_eq!(
			(errors[0].token.line, errors[0].token.column),
			(line, column)
		);
	}

	#[test]
	fn a_malformed_rate_prefix_avoids_a_panic() {
		let (commands, errors) = tape::parse(r#"Type@0.0.0.0s "hi""#);
		assert!(!errors.is_empty());
		assert!(!commands.iter().any(|c| matches!(c, Commands::Type { .. })));
	}

	/// A keypress repeat count too large for `u32` silently falls back to one,
	/// unlike a setting's value — a real quirk of the language, pinned here.
	#[test]
	fn a_repeat_count_too_large_for_u32_falls_back_to_one() {
		assert!(matches!(
			one("Up 99999999999999999999"),
			Commands::Key {
				repeat_count: 1,
				..
			}
		));
	}

	// ---- The malformed-input corpus --------------------------------------

	/// Every one of these is broken differently. None may panic, and none may
	/// leave a command behind that was built from the broken input.
	#[rstest]
	#[case("Type")]
	#[case("Ctrl")]
	#[case("Alt")]
	#[case("Env")]
	#[case("Require")]
	#[case("Wait+")]
	#[case("Output")]
	#[case("Frobnicate")]
	#[case("Set FontSize")]
	#[case("Wait/(/")]
	#[case("Screenshot \"abc")]
	#[case("\u{1f4a5}")]
	fn a_corpus_of_malformed_tapes_never_smuggles_out_a_command(#[case] source: &str) {
		rejected(source);
	}

	/// `Sleep 0.0.0.0` is the one malformed case that still yields a command —
	/// a `Sleep` with *no* duration — alongside its error, rather than none.
	/// It must never report a duration built from the unparsable number.
	#[test]
	fn a_malformed_number_reports_an_error_instead_of_panicking() {
		let (commands, errors) = tape::parse("Sleep 0.0.0.0");
		assert!(!errors.is_empty());
		assert!(
			!commands
				.iter()
				.any(|c| matches!(c, Commands::Sleep { duration: Some(_) }))
		);
	}

	#[test]
	fn a_deeply_malformed_modifier_chain_is_rejected_without_overflowing() {
		rejected(&format!("Ctrl+c{}", "+Alt".repeat(3_000)));
	}

	#[rstest]
	#[case("")]
	#[case("# nothing to see here\n# still nothing")]
	fn an_empty_or_comment_only_tape_yields_nothing(#[case] source: &str) {
		let (commands, errors) = tape::parse(source);
		assert!(
			commands.is_empty() && errors.is_empty(),
			"{commands:?} {errors:?}"
		);
	}
}
