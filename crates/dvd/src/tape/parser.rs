// src/parser.rs
use super::lexer::Lexer;
use super::token::{KEYWORDS, Token, TokenType, is_modifier, is_setting};
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

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TypeCommand {
	pub rate: Option<Duration>,
	pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SleepCommand {
	pub duration: Option<Duration>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct OutputCommand {
	pub path: PathBuf,
	pub format: Output,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct KeyCommand {
	pub key: TokenType,
	pub rate: Option<Duration>,
	pub repeat_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CtrlCommand {
	pub keys: Vec<String>, // e.g., ["c"] for Ctrl+C, ["alt", "tab"] for Ctrl+Alt+Tab
	pub rate: Option<Duration>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SetCommand {
	pub setting: Setting,
}

impl Default for Setting {
	fn default() -> Self {
		Setting::CursorBlink(false)
	}
}
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequireCommand {
	pub program: String,
}

#[derive(Debug, Default, Clone)]
pub struct WaitCommand {
	pub mode: WaitMode,
	pub pattern: Option<Regex>, // regex pattern
	pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum WaitMode {
	#[default]
	Line,
	Screen,
}

impl FromStr for WaitMode {
	type Err = Error;

	fn from_str(input: &str) -> std::result::Result<Self, Self::Err> {
		// Normalize the string
		let normalized = input.to_lowercase();

		match normalized.as_str() {
			"line" => Ok(WaitMode::Line),
			"screen" => Ok(WaitMode::Screen),
			_ => Err(anyhow!(
				"Wait mode '{}' not recognized. Valid options: line, screen",
				input
			)),
		}
	}
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScreenshotCommand {
	pub path: PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CopyCommand {
	pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct EnvCommand {
	pub variable: String,
	pub value: String,
}

#[derive(Debug, Clone)]
pub enum Commands {
	Type(TypeCommand),
	Sleep(SleepCommand),
	Output(OutputCommand),
	Key(KeyCommand),
	Ctrl(CtrlCommand),
	Alt(CtrlCommand),
	Shift(CtrlCommand),
	Set(SetCommand),
	Require(RequireCommand),
	Wait(WaitCommand),
	Screenshot(ScreenshotCommand),
	Copy(CopyCommand),
	Paste, // No additional data needed
	Env(EnvCommand),
	Hide, // No additional data needed
	Show, // No additional data needed
}

impl From<TypeCommand> for Commands {
	fn from(cmd: TypeCommand) -> Self {
		Commands::Type(cmd)
	}
}

impl From<SleepCommand> for Commands {
	fn from(cmd: SleepCommand) -> Self {
		Commands::Sleep(cmd)
	}
}

impl From<OutputCommand> for Commands {
	fn from(cmd: OutputCommand) -> Self {
		Commands::Output(cmd)
	}
}

impl From<KeyCommand> for Commands {
	fn from(cmd: KeyCommand) -> Self {
		Commands::Key(cmd)
	}
}

impl From<SetCommand> for Commands {
	fn from(cmd: SetCommand) -> Self {
		Commands::Set(cmd)
	}
}

impl From<RequireCommand> for Commands {
	fn from(cmd: RequireCommand) -> Self {
		Commands::Require(cmd)
	}
}

impl From<WaitCommand> for Commands {
	fn from(cmd: WaitCommand) -> Self {
		Commands::Wait(cmd)
	}
}

impl From<ScreenshotCommand> for Commands {
	fn from(cmd: ScreenshotCommand) -> Self {
		Commands::Screenshot(cmd)
	}
}

impl From<CopyCommand> for Commands {
	fn from(cmd: CopyCommand) -> Self {
		Commands::Copy(cmd)
	}
}

impl From<EnvCommand> for Commands {
	fn from(cmd: EnvCommand) -> Self {
		Commands::Env(cmd)
	}
}

impl From<()> for Commands {
	fn from(_: ()) -> Self {
		Commands::Paste
	}
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

		// Read at least two tokens so current_token and peek_token are both set
		parser.next_token();
		parser.next_token();

		parser
	}

	pub fn parse(&mut self) -> Vec<Commands> {
		let mut commands = Vec::new();

		while self.current_token.token_type != TokenType::Eof {
			// Skipping comments
			if self.current_token.token_type == TokenType::Comment {
				self.next_token();
				continue;
			}

			match self.get_current_command() {
				Ok(cmds) => commands.push(cmds),
				Err(e) => {
					self.errors.push(ParseError {
						token: self.current_token.clone(),
						message: e.to_string(),
					});
				}
			}
			self.next_token();
		}

		commands
	}

	/// Get an array of the current errors.
	pub fn errors(&self) -> &[ParseError] {
		&self.errors
	}

	fn get_current_command(&mut self) -> Result<Commands> {
		match self.current_token.token_type {
			TokenType::Space
			| TokenType::Backspace
			| TokenType::Delete
			| TokenType::Insert
			| TokenType::Enter
			| TokenType::Escape
			| TokenType::Tab
			| TokenType::Home
			| TokenType::End
			| TokenType::Down
			| TokenType::Left
			| TokenType::Right
			| TokenType::Up
			| TokenType::PageUp
			| TokenType::PageDown => Ok(self
				.parse_keypress(self.current_token.token_type.clone())
				.into()),
			TokenType::Set => Ok(self.parse_set()?.into()),
			TokenType::Output => Ok(self.parse_output()?.into()),
			TokenType::Sleep => Ok(self.parse_sleep()?.into()),
			TokenType::Type => Ok(self.parse_type()?.into()),
			// `parse_ctrl`, `parse_alt`, and `parse_shift` all hand back the
			// same `CtrlCommand` struct. A `From<CtrlCommand> for Commands`
			// impl used to sit here and every one of the three call sites
			// leaned on it through `.into()` — but a single `From` impl can
			// only pick one of `Commands::Ctrl`/`Alt`/`Shift`, so `Alt+Enter`
			// and `Shift+Tab` were both silently reaching `burn` wrapped as
			// `Commands::Ctrl`. Wrapping explicitly here is the fix; the
			// `From` impl was deleted rather than left as a trap for the
			// next call site that reaches for `.into()` out of habit.
			TokenType::Ctrl => Ok(Commands::Ctrl(self.parse_ctrl()?)),
			TokenType::Alt => Ok(Commands::Alt(self.parse_alt()?)),
			TokenType::Shift => Ok(Commands::Shift(self.parse_shift()?)),
			TokenType::Hide => Ok(Commands::Hide),
			TokenType::Require => Ok(self.parse_require()?.into()),
			TokenType::Show => Ok(Commands::Show),
			TokenType::Wait => Ok(self.parse_wait()?.into()),
			TokenType::Screenshot => Ok(self.parse_screenshot()?.into()),
			TokenType::Copy => Ok(self.parse_copy()?.into()),
			TokenType::Paste => Ok(Commands::Paste),
			TokenType::Env => Ok(self.parse_env()?.into()),
			_ => Err(anyhow!("Invalid command: {}", self.current_token.literal)),
		}
	}

	fn parse_wait(&mut self) -> Result<WaitCommand> {
		let mut cmd = WaitCommand::default();

		if self.peek_token.token_type == TokenType::Plus {
			self.next_token();
			if self.peek_token.token_type != TokenType::String
				|| (self.peek_token.literal != "Line" && self.peek_token.literal != "Screen")
			{
				return Err(anyhow!("Wait+ expects Line or Screen"));
			}
			cmd.mode = self.peek_token.literal.clone().parse()?;
			self.next_token();
		} else {
			cmd.mode = WaitMode::Line;
		}

		let speed = self.parse_speed();
		if speed != Duration::default() {
			cmd.timeout = Some(speed);
		}

		// Handle wait regex
		if self.peek_token.token_type == TokenType::Regex {
			self.next_token();
			// Make sure it's valid
			if Regex::new(&self.current_token.literal).is_err() {
				return Err(anyhow!(
					"Invalid regular expression '{}': invalid regex",
					self.current_token.literal
				));
			}

			// Assign the built regex
			cmd.pattern = Some(self.current_token.literal.clone().parse()?);
		}

		Ok(cmd)
	}

	fn parse_speed(&mut self) -> Duration {
		if self.peek_token.token_type == TokenType::At {
			self.next_token(); // consume the '@'
			self.parse_time()
		} else {
			Duration::default()
		}
	}

	fn parse_repeat(&mut self) -> u32 {
		if self.peek_token.token_type == TokenType::Number {
			let count: u32 = self.peek_token.literal.parse().unwrap_or(1);
			self.next_token();
			count
		} else {
			1
		}
	}

	/// Helper function that gets the corresponding duration from a time
	///
	/// The lexer's `read_number` accepts any run of digits and dots — that is
	/// what lets `1.5s` lex as one token — so it can also hand back something
	/// no `f64` can parse, like `0.0.0.0`. Falling back to a recorded
	/// [`ParseError`] instead of `.unwrap()`ing that parse is the difference
	/// between a rejected tape and a crashed one.
	fn parse_time(&mut self) -> Duration {
		// get the user provided integer value for the time
		let provided_time: f64 = if self.peek_token.token_type == TokenType::Number {
			let base = self.peek_token.literal.clone();
			self.next_token(); // consume the number
			match base.parse() {
				Ok(value) => value,
				Err(_) => {
					self.errors.push(ParseError {
						token: self.current_token.clone(),
						message: format!("{base:?} is not a valid time value"),
					});
					return Duration::default();
				}
			}
		} else {
			// If the next token is not a number, this is invalid.
			self.errors.push(ParseError {
				token: self.current_token.clone(),
				message: format!("Expected time after {}", self.current_token.literal),
			});
			return Duration::default();
		};

		// Check for time unit and create Duration accordingly
		if matches!(
			self.peek_token.token_type,
			TokenType::Milliseconds | TokenType::Seconds | TokenType::Minutes
		) {
			let duration = match self.peek_token.token_type {
				TokenType::Milliseconds => Duration::from_millis(provided_time as u64),
				TokenType::Seconds => Duration::from_secs(provided_time as u64),
				TokenType::Minutes => Duration::from_secs((provided_time * 60.0) as u64),
				_ => unreachable!(), // We should have already matched above
			};
			self.next_token(); // Advance past the time unit token
			duration
		} else {
			// Default to seconds if no marker is denoted
			Duration::from_secs(provided_time as u64)
		}
	}

	fn parse_ctrl(&mut self) -> Result<CtrlCommand> {
		// optional @<time>
		let dur = self.parse_speed();
		let rate = if dur != Duration::default() {
			Some(dur)
		} else {
			None
		};

		let mut keys = Vec::new();
		let mut in_modifier_chain = true;

		// expect a series of "+X" tokens
		while self.peek_token.token_type == TokenType::Plus {
			self.next_token(); // consume the '+'
			let peek = &self.peek_token;

			// is this a named modifier? (Alt or Shift)
			if KEYWORDS.get(&*peek.literal).is_some_and(is_modifier) {
				if !in_modifier_chain {
					return Err(anyhow!("Modifiers must come before other keys"));
				}
				keys.push(peek.literal.clone());
				self.next_token();
				continue;
			}

			// once we hit a non-modifier, no more modifiers allowed
			in_modifier_chain = false;

			// must be a single‐char string or one of the special keys
			let lit = peek.literal.clone();
			match peek.token_type {
				TokenType::Enter
				| TokenType::Space
				| TokenType::Backspace
				| TokenType::Delete
				| TokenType::Insert
				| TokenType::Tab
				| TokenType::Escape
				| TokenType::Minus
				| TokenType::At
				| TokenType::LeftBracket
				| TokenType::RightBracket
				| TokenType::Caret
				| TokenType::Backslash => keys.push(lit),
				TokenType::String if lit.len() == 1 => keys.push(lit),
				_ => return Err(anyhow!("Invalid Ctrl key: {}", lit)),
			}

			self.next_token(); // consume the actual key
		}

		if keys.is_empty() {
			return Err(anyhow!("Expected at least one key after Ctrl"));
		}

		Ok(CtrlCommand { keys, rate })
	}

	fn parse_alt(&mut self) -> Result<CtrlCommand> {
		// optional @<time> prefix
		let dur = self.parse_speed();
		let rate = if dur != Duration::default() {
			Some(dur)
		} else {
			None
		};

		// must be "+<key>"
		if self.peek_token.token_type != TokenType::Plus {
			return Err(anyhow!(
				"Expected '+' after Alt, got {}",
				self.peek_token.literal
			));
		}
		self.next_token(); // consume '+'

		// validate the one key
		let peek = &self.peek_token;
		let ok = matches!(
			peek.token_type,
			TokenType::String
				| TokenType::Enter
				| TokenType::LeftBracket
				| TokenType::RightBracket
				| TokenType::Tab
		);
		if !ok {
			return Err(anyhow!("Invalid Alt key: {}", peek.literal));
		}

		let key = peek.literal.clone();
		self.next_token(); // consume the key

		Ok(CtrlCommand {
			keys: vec![key],
			rate,
		})
	}

	fn parse_shift(&mut self) -> Result<CtrlCommand> {
		// optional @<time> prefix
		let dur = self.parse_speed();
		let rate = if dur != Duration::default() {
			Some(dur)
		} else {
			None
		};

		// must be "+<key>"
		if self.peek_token.token_type != TokenType::Plus {
			return Err(anyhow!(
				"Expected '+' after Shift, got {}",
				self.peek_token.literal
			));
		}
		self.next_token(); // consume '+'

		// validate the one key
		let peek = &self.peek_token;
		let ok = matches!(
			peek.token_type,
			TokenType::String
				| TokenType::Enter
				| TokenType::LeftBracket
				| TokenType::RightBracket
				| TokenType::Tab
		);
		if !ok {
			return Err(anyhow!("Invalid Shift key: {}", peek.literal));
		}

		let key = peek.literal.clone();
		self.next_token(); // consume the key

		Ok(CtrlCommand {
			keys: vec![key],
			rate,
		})
	}

	fn parse_keypress(&mut self, command_type: TokenType) -> KeyCommand {
		let mut cmd = KeyCommand::default();

		let speed = self.parse_speed();
		if speed != Duration::default() {
			cmd.rate = Some(speed);
		} // Otherwise this stays None

		cmd.repeat_count = self.parse_repeat();

		cmd.key = command_type; // Set the key
		cmd
	}

	fn parse_output(&mut self) -> Result<OutputCommand> {
		let mut cmd = OutputCommand::default();

		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("Expected file path after output"));
		}

		let path = Path::new(&self.peek_token.literal);
		cmd.format = match path.extension() {
			// Rejected here, at parse time, rather than left for `burn` to
			// discover after the PTY is already open: a `.check` run should
			// catch "Output out.gif" as surely as a missing quote.
			Some(ext) => ext
				.to_string_lossy()
				.parse::<Output>()
				.map_err(|message| anyhow!(message))?,
			None => {
				if !self.peek_token.literal.ends_with('/') {
					return Err(anyhow!("Expected folder with trailing slash"));
				}
				Output::Png
			}
		};

		// Parse the path from the next token (Should be the path)
		cmd.path = PathBuf::from(self.peek_token.literal.clone());
		self.next_token();
		Ok(cmd)
	}

	fn parse_set(&mut self) -> Result<SetCommand> {
		// Make sure the next token really is a setting name
		if !is_setting(&self.peek_token.token_type) {
			return Err(anyhow!("Unknown setting: {}", self.peek_token.literal));
		}

		// Remember which setting, then consume it
		let setting_type = self.peek_token.token_type.clone();
		self.next_token(); // now current_token is the setting name, peek_token is its value

		// Parse the value according to which setting it is
		let setting = match setting_type {
			TokenType::Shell => {
				// can be a JSON literal or a bare string
				if !matches!(
					self.peek_token.token_type,
					TokenType::String | TokenType::Json
				) {
					return Err(anyhow!(
						"Set Shell expects string or JSON, got {}",
						self.peek_token.literal
					));
				}
				let val = self.peek_token.literal.clone();
				self.next_token();
				Setting::Shell(val)
			}

			TokenType::FontSize => {
				let size: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::FontSize(size)
			}

			TokenType::FontFamily => {
				let fam = self.peek_token.literal.clone();
				self.next_token();
				Setting::FontFamily(fam)
			}

			TokenType::Width => {
				let w: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::Width(w)
			}

			TokenType::Height => {
				let h: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::Height(h)
			}

			TokenType::LetterSpacing => {
				let ls: f32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::LetterSpacing(ls)
			}

			TokenType::LineHeight => {
				let lh: f32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::LineHeight(lh)
			}

			TokenType::LoopOffset => {
				// might be "25" + "%" token, or "25%"
				let mut lit = self.peek_token.literal.clone();
				self.next_token();
				if self.peek_token.token_type == TokenType::Percent {
					self.next_token(); // consume '%'
				}
				if lit.ends_with('%') {
					lit.pop();
				}
				let v: f32 = lit.parse()?;
				Setting::LoopOffset(v)
			}

			TokenType::Theme => {
				let theme = self.peek_token.literal.clone();
				self.next_token();
				Setting::Theme(theme)
			}

			TokenType::Padding => {
				let p: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::Padding(p)
			}

			TokenType::Framerate => {
				let fr: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::Framerate(fr)
			}

			TokenType::PlaybackSpeed => {
				let ps: f32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::PlaybackSpeed(ps)
			}

			TokenType::MarginFill => {
				let mf = self.peek_token.literal.clone();
				self.next_token();
				Setting::MarginFill(mf)
			}

			TokenType::Margin => {
				let m: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::Margin(m)
			}

			TokenType::BorderRadius => {
				let br: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::BorderRadius(br)
			}

			TokenType::WindowBar => {
				let wb = self.peek_token.literal.clone();
				self.next_token();
				Setting::WindowBar(wb)
			}

			TokenType::WindowBarSize => {
				let wbs: u32 = self.peek_token.literal.parse()?;
				self.next_token();
				Setting::WindowBarSize(wbs)
			}

			TokenType::TypingSpeed => {
				// expect a number then optional ms|s
				if self.peek_token.token_type != TokenType::Number {
					return Err(anyhow!(
						"Set TypingSpeed expects a number, got {}",
						self.peek_token.literal
					));
				}
				let val: f64 = self.peek_token.literal.parse()?;
				self.next_token();
				let dur = if matches!(
					self.peek_token.token_type,
					TokenType::Milliseconds | TokenType::Seconds
				) {
					let unit = self.peek_token.token_type.clone();
					self.next_token();
					match unit {
						TokenType::Milliseconds => Duration::from_millis(val as u64),
						TokenType::Seconds => Duration::from_secs(val as u64),
						_ => unreachable!(),
					}
				} else {
					Duration::from_secs(val as u64)
				};
				Setting::TypingSpeed(dur)
			}

			TokenType::WaitTimeout => {
				// number then ms|s|m
				if self.peek_token.token_type != TokenType::Number {
					return Err(anyhow!(
						"Set WaitTimeout expects a number, got {}",
						self.peek_token.literal
					));
				}
				let val: f64 = self.peek_token.literal.parse()?;
				self.next_token();
				let dur = if matches!(
					self.peek_token.token_type,
					TokenType::Milliseconds | TokenType::Seconds | TokenType::Minutes
				) {
					let unit = self.peek_token.token_type.clone();
					self.next_token();
					match unit {
						TokenType::Milliseconds => Duration::from_millis(val as u64),
						TokenType::Seconds => Duration::from_secs(val as u64),
						TokenType::Minutes => Duration::from_secs((val * 60.0) as u64),
						_ => unreachable!(),
					}
				} else {
					Duration::from_secs(val as u64)
				};
				Setting::WaitTimeout(dur)
			}

			TokenType::WaitPattern => {
				let pat = self.peek_token.literal.clone();
				if Regex::new(&pat).is_err() {
					return Err(anyhow!("Invalid regexp pattern: {}", pat));
				}
				self.next_token();
				Setting::WaitPattern(pat)
			}

			TokenType::CursorBlink => {
				let lit = self.peek_token.literal.clone();
				let b = match lit.as_str() {
					"true" => true,
					"false" => false,
					_ => return Err(anyhow!("Set CursorBlink expects true/false, got {}", lit)),
				};
				self.next_token();
				Setting::CursorBlink(b)
			}

			// We’ve already guarded with is_setting, so nothing else can happen:
			_ => unreachable!(),
		};

		Ok(SetCommand { setting })
	}

	fn parse_sleep(&mut self) -> Result<SleepCommand> {
		let duration = if self.peek_token.token_type == TokenType::Number {
			self.parse_time()
		} else {
			Duration::default()
		};

		let mut cmd = SleepCommand::default();

		if duration != Duration::default() {
			cmd.duration = Some(duration);
		} else {
			// If no duration is specified, it's None
			cmd.duration = None;
		}

		Ok(cmd)
	}

	fn parse_require(&mut self) -> Result<RequireCommand> {
		let mut cmd = RequireCommand::default();

		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects one string", self.current_token.literal));
		}

		cmd.program = self.peek_token.literal.clone();
		self.next_token();
		Ok(cmd)
	}

	fn parse_type(&mut self) -> Result<TypeCommand> {
		let mut cmd = TypeCommand::default();

		let speed = self.parse_speed();
		if speed != Duration::default() {
			cmd.rate = Some(speed);
		}

		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}

		// Adjacent string literals concatenate, so `Type "cd " "/tmp"` types one
		// line. Assigning here instead of appending would keep only the last.
		while self.peek_token.token_type == TokenType::String {
			self.next_token();
			cmd.text.push_str(&self.current_token.literal);
		}

		Ok(cmd)
	}

	fn parse_copy(&mut self) -> Result<CopyCommand> {
		let mut cmd = CopyCommand::default();

		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}

		let mut text = String::new();
		while self.peek_token.token_type == TokenType::String {
			self.next_token();
			text.push_str(&self.current_token.literal.clone());
		}

		cmd.text = text;
		Ok(cmd)
	}

	fn parse_env(&mut self) -> Result<EnvCommand> {
		// The name is whatever the lexer made of the bare word, so it arrives as
		// a `String` unless it collides with a keyword — `Env Set "x"` should
		// fail here rather than silently export a variable called "Set".
		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!(
				"Env expects a variable name, got {}",
				self.peek_token.literal
			));
		}

		let mut cmd = EnvCommand {
			variable: self.peek_token.literal.clone(),
			..Default::default()
		};

		self.next_token();

		if self.peek_token.token_type != TokenType::String {
			return Err(anyhow!("{} expects string", self.current_token.literal));
		}

		// Then the value the user wants assigned to it.
		cmd.value = self.peek_token.literal.clone();
		self.next_token();

		Ok(cmd)
	}

	fn parse_screenshot(&mut self) -> Result<ScreenshotCommand> {
		let mut cmd = ScreenshotCommand::default();

		if self.peek_token.token_type != TokenType::String {
			self.next_token();
			return Err(anyhow!("Expected path after Screenshot"));
		}

		let path = Path::new(&self.peek_token.literal);
		if path.extension().is_none_or(|ext| ext != "png") {
			self.next_token();
			return Err(anyhow!("Expected file with .png extension"));
		}

		cmd.path = PathBuf::from(self.peek_token.literal.clone());
		self.next_token();
		Ok(cmd)
	}

	fn next_token(&mut self) {
		self.current_token = self.peek_token.clone();
		self.peek_token = self.lexer.next_token();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tape;

	/// Parse a tape that is expected to be valid, panicking with the errors if
	/// it was not. Most of this module wants to assert something about the
	/// shape of the commands, not re-litigate whether the source parses.
	fn parse_ok(source: &str) -> Vec<Commands> {
		let (commands, errors) = tape::parse(source);
		assert!(
			errors.is_empty(),
			"unexpected parse errors for {source:?}: {errors:?}"
		);
		commands
	}

	/// Parse a tape expected to yield exactly one command, and hand back that
	/// command directly so a test can destructure it in one line.
	fn one(source: &str) -> Commands {
		let mut commands = parse_ok(source);
		assert_eq!(
			commands.len(),
			1,
			"expected exactly one command from {source:?}, got {commands:?}"
		);
		commands.remove(0)
	}

	// ---- Keys ---------------------------------------------------------

	#[test]
	fn every_named_key_reaches_its_token() {
		let cases = [
			("Enter", TokenType::Enter),
			("Tab", TokenType::Tab),
			("Space", TokenType::Space),
			("Backspace", TokenType::Backspace),
			("Escape", TokenType::Escape),
			("Delete", TokenType::Delete),
			("Insert", TokenType::Insert),
			("Up", TokenType::Up),
			("Down", TokenType::Down),
			("Left", TokenType::Left),
			("Right", TokenType::Right),
			("PageUp", TokenType::PageUp),
			("PageDown", TokenType::PageDown),
			("Home", TokenType::Home),
			("End", TokenType::End),
		];

		for (source, expected) in cases {
			let Commands::Key(key) = one(source) else {
				panic!("{source} did not parse as a Key command");
			};
			assert_eq!(key.key, expected, "{source}");
			assert_eq!(key.repeat_count, 1, "{source}");
			assert_eq!(key.rate, None, "{source}");
		}
	}

	#[test]
	fn a_keypress_takes_a_repeat_count_after_its_optional_rate() {
		let Commands::Key(key) = one("Up 5") else {
			panic!("expected a Key command");
		};
		assert_eq!(key.repeat_count, 5);
		assert_eq!(key.rate, None);

		let Commands::Key(key) = one("Up@20ms 5") else {
			panic!("expected a Key command");
		};
		assert_eq!(key.repeat_count, 5);
		assert_eq!(key.rate, Some(Duration::from_millis(20)));
	}

	// ---- The @<time> rate prefix ---------------------------------------

	#[test]
	fn the_rate_prefix_sets_a_duration_on_every_command_that_accepts_one() {
		let Commands::Type(typed) = one(r#"Type@10ms "x""#) else {
			panic!("expected a Type command");
		};
		assert_eq!(typed.rate, Some(Duration::from_millis(10)));

		let Commands::Ctrl(ctrl) = one("Ctrl@10ms+c") else {
			panic!("expected a Ctrl command");
		};
		assert_eq!(ctrl.rate, Some(Duration::from_millis(10)));

		let Commands::Alt(alt) = one("Alt@10ms+a") else {
			panic!("expected an Alt command");
		};
		assert_eq!(alt.rate, Some(Duration::from_millis(10)));

		let Commands::Shift(shift) = one("Shift@10ms+a") else {
			panic!("expected a Shift command");
		};
		assert_eq!(shift.rate, Some(Duration::from_millis(10)));

		let Commands::Key(key) = one("Up@10ms") else {
			panic!("expected a Key command");
		};
		assert_eq!(key.rate, Some(Duration::from_millis(10)));
	}

	/// `WaitCommand` calls the same field `timeout` rather than `rate`: unlike
	/// the others, `@<time>` on `Wait` is not pacing a repeated keystroke, it
	/// is a ceiling on how long to wait at all.
	#[test]
	fn wait_takes_its_rate_prefix_as_a_timeout_not_a_rate() {
		let Commands::Wait(wait) = one("Wait@250ms") else {
			panic!("expected a Wait command");
		};
		assert_eq!(wait.timeout, Some(Duration::from_millis(250)));
	}

	// ---- Ctrl / Alt / Shift modifier chains ----------------------------

	#[test]
	fn ctrl_chains_accept_alt_and_shift_as_leading_modifiers() {
		let Commands::Ctrl(ctrl) = one("Ctrl+Alt+c") else {
			panic!("expected a Ctrl command");
		};
		assert_eq!(ctrl.keys, vec!["Alt".to_string(), "c".to_string()]);

		let Commands::Ctrl(ctrl) = one("Ctrl+Shift+a") else {
			panic!("expected a Ctrl command");
		};
		assert_eq!(ctrl.keys, vec!["Shift".to_string(), "a".to_string()]);
	}

	/// Once a non-modifier key appears, the chain is closed. Without this
	/// rule `Ctrl+c+Alt` would either silently reorder into a sensible chord
	/// or send a keystroke nobody asked for; refusing it is the honest
	/// answer.
	#[test]
	fn modifiers_must_precede_other_keys_in_a_ctrl_chain() {
		let (commands, errors) = tape::parse("Ctrl+c+Alt");
		assert!(!errors.is_empty(), "expected a parse error");
		assert!(
			!commands.iter().any(|c| matches!(c, Commands::Ctrl(_))),
			"no Ctrl command should survive: {commands:?}"
		);
	}

	#[test]
	fn alt_and_shift_require_exactly_one_key_after_a_plus() {
		let Commands::Alt(alt) = one("Alt+Enter") else {
			panic!("expected an Alt command");
		};
		assert_eq!(alt.keys, vec!["Enter".to_string()]);

		let Commands::Shift(shift) = one("Shift+Tab") else {
			panic!("expected a Shift command");
		};
		assert_eq!(shift.keys, vec!["Tab".to_string()]);

		let (commands, errors) = tape::parse("Alt a");
		assert!(!errors.is_empty(), "Alt without '+' should be rejected");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Alt(_))));
	}

	// ---- Wait -----------------------------------------------------------

	#[test]
	fn wait_plus_selects_line_or_screen_mode() {
		let Commands::Wait(wait) = one("Wait") else {
			panic!("expected a Wait command");
		};
		assert_eq!(wait.mode, WaitMode::Line, "bare Wait defaults to Line");

		let Commands::Wait(wait) = one("Wait+Line") else {
			panic!("expected a Wait command");
		};
		assert_eq!(wait.mode, WaitMode::Line);

		let Commands::Wait(wait) = one("Wait+Screen") else {
			panic!("expected a Wait command");
		};
		assert_eq!(wait.mode, WaitMode::Screen);
	}

	/// A space is required before the pattern here, and that is a real quirk
	/// rather than a stylistic choice: `read_identifier` treats `/` as an
	/// ordinary identifier character (so an unquoted `Output stills/` lexes
	/// as one token naming a directory), which means a `/` glued directly
	/// onto a preceding bare word — `Wait/ready/`, or a unit suffix in
	/// `2s/ready/` — gets swallowed into that word instead of starting a
	/// fresh `Regex` token. See
	/// `a_wait_regex_glued_to_the_keyword_is_not_recognised` below for the
	/// failing case pinned down as a known bug rather than fixed here: the
	/// fix would have to make `/` context-sensitive in a lexer whose whole
	/// design is not being that, and untangling it is bigger than this
	/// workstream's remit.
	#[test]
	fn wait_accepts_a_trailing_regex_pattern() {
		let Commands::Wait(wait) = one("Wait /ready/") else {
			panic!("expected a Wait command");
		};
		let pattern = wait.pattern.expect("a pattern should have been parsed");
		assert!(pattern.is_match("ready"));
	}

	#[test]
	fn wait_combines_mode_timeout_and_pattern_in_one_command() {
		let Commands::Wait(wait) = one("Wait+Screen@2s /ready/") else {
			panic!("expected a Wait command");
		};
		assert_eq!(wait.mode, WaitMode::Screen);
		assert_eq!(wait.timeout, Some(Duration::from_secs(2)));
		assert!(
			wait.pattern
				.expect("a pattern should have been parsed")
				.is_match("ready")
		);
	}

	/// The bug `wait_accepts_a_trailing_regex_pattern`'s doc comment
	/// describes, pinned down rather than left implicit: with no space,
	/// `Wait/ready/` lexes as a single bare-word token (`read_identifier`
	/// happily consumes `/` as just another identifier character), so the
	/// lexer never hands the parser a `Wait` keyword token at all — the
	/// whole thing reads as an unrecognised command instead of a `Wait`
	/// with a pattern. Recorded here so a future fix has a test to turn
	/// green instead of a bug report to rediscover from scratch.
	#[test]
	fn a_wait_regex_glued_to_the_keyword_is_not_recognised() {
		let (commands, errors) = tape::parse("Wait/ready/");
		assert!(!errors.is_empty(), "{commands:?}");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Wait(_))));
	}

	// ---- Sleep and time literals -----------------------------------------

	#[test]
	fn sleep_accepts_every_time_unit() {
		let Commands::Sleep(sleep) = one("Sleep") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(
			sleep.duration, None,
			"a bare Sleep has no explicit duration"
		);

		let Commands::Sleep(sleep) = one("Sleep 500ms") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(sleep.duration, Some(Duration::from_millis(500)));

		let Commands::Sleep(sleep) = one("Sleep 2s") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(sleep.duration, Some(Duration::from_secs(2)));

		let Commands::Sleep(sleep) = one("Sleep 1m") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(sleep.duration, Some(Duration::from_secs(60)));

		let Commands::Sleep(sleep) = one("Sleep 3") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(
			sleep.duration,
			Some(Duration::from_secs(3)),
			"no unit defaults to seconds"
		);
	}

	/// `parse_time` builds its `Duration` with `provided_time as u64`, so the
	/// fraction below one whole second disappears instead of rounding —
	/// `Sleep 1.9s` sleeps for one second, not two. Documented here so a
	/// change to that cast is a deliberate decision, not a surprise.
	#[test]
	fn fractional_seconds_truncate_toward_zero() {
		let Commands::Sleep(sleep) = one("Sleep 1.9s") else {
			panic!("expected a Sleep command");
		};
		assert_eq!(sleep.duration, Some(Duration::from_secs(1)));
	}

	// ---- Type / Copy string handling -------------------------------------

	#[test]
	fn type_captures_plain_text_without_a_rate() {
		let Commands::Type(typed) = one(r#"Type "hi""#) else {
			panic!("expected a Type command");
		};
		assert_eq!(typed.text, "hi");
		assert_eq!(typed.rate, None);
	}

	/// `parse_copy` re-implements the same "adjacent strings concatenate"
	/// rule `parse_type` has. A change to one that forgets the other would
	/// silently truncate `Copy "a" "b"` to `"a"` while `Type "a" "b"` kept
	/// working.
	#[test]
	fn adjacent_string_literals_concatenate_in_copy_too() {
		let Commands::Copy(copy) = one(r#"Copy "a" "b""#) else {
			panic!("expected a Copy command");
		};
		assert_eq!(copy.text, "ab");
	}

	// ---- Set / Setting ----------------------------------------------------

	#[test]
	fn every_setting_parses_its_declared_value() {
		let Commands::Set(set) = one(r#"Set Shell "zsh""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Shell("zsh".to_string()));

		let Commands::Set(set) = one(r#"Set Shell {"cmd":"zsh"}"#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Shell(r#"{"cmd":"zsh"}"#.to_string()));

		let Commands::Set(set) = one("Set FontSize 24") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::FontSize(24));

		let Commands::Set(set) = one(r#"Set FontFamily "Fira Code""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::FontFamily("Fira Code".to_string()));

		let Commands::Set(set) = one("Set Width 1200") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Width(1200));

		let Commands::Set(set) = one("Set Height 600") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Height(600));

		let Commands::Set(set) = one("Set LetterSpacing 0.5") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::LetterSpacing(0.5));

		let Commands::Set(set) = one("Set LineHeight 1.2") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::LineHeight(1.2));

		let Commands::Set(set) = one("Set LoopOffset 25%") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::LoopOffset(25.0));

		let Commands::Set(set) = one("Set LoopOffset 10") else {
			panic!("expected a Set command");
		};
		assert_eq!(
			set.setting,
			Setting::LoopOffset(10.0),
			"the '%' suffix is optional"
		);

		let Commands::Set(set) = one(r#"Set Theme "nord""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Theme("nord".to_string()));

		let Commands::Set(set) = one("Set Padding 20") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Padding(20));

		let Commands::Set(set) = one("Set Framerate 60") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Framerate(60));

		let Commands::Set(set) = one("Set PlaybackSpeed 1.5") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::PlaybackSpeed(1.5));

		let Commands::Set(set) = one(r#"Set MarginFill "black""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::MarginFill("black".to_string()));

		let Commands::Set(set) = one("Set Margin 10") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::Margin(10));

		let Commands::Set(set) = one("Set BorderRadius 8") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::BorderRadius(8));

		let Commands::Set(set) = one(r#"Set WindowBar "Colorful""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::WindowBar("Colorful".to_string()));

		let Commands::Set(set) = one("Set WindowBarSize 40") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::WindowBarSize(40));

		let Commands::Set(set) = one("Set TypingSpeed 2") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::TypingSpeed(Duration::from_secs(2)));

		let Commands::Set(set) = one("Set TypingSpeed 50ms") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::TypingSpeed(Duration::from_millis(50)));

		let Commands::Set(set) = one("Set WaitTimeout 5") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::WaitTimeout(Duration::from_secs(5)));

		let Commands::Set(set) = one("Set WaitTimeout 500ms") else {
			panic!("expected a Set command");
		};
		assert_eq!(
			set.setting,
			Setting::WaitTimeout(Duration::from_millis(500))
		);

		let Commands::Set(set) = one("Set WaitTimeout 2m") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::WaitTimeout(Duration::from_secs(120)));

		let Commands::Set(set) = one(r#"Set WaitPattern "ready""#) else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::WaitPattern("ready".to_string()));

		let Commands::Set(set) = one("Set CursorBlink true") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::CursorBlink(true));

		let Commands::Set(set) = one("Set CursorBlink false") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::CursorBlink(false));
	}

	#[test]
	fn set_rejects_a_name_that_is_not_a_known_setting() {
		let (commands, errors) = tape::parse("Set Nonsense 1");
		assert!(!errors.is_empty());
		assert!(!commands.iter().any(|c| matches!(c, Commands::Set(_))));
	}

	/// The lexer always hands `%` back as its own [`TokenType::Percent`]
	/// token — `read_number` stops at the first non-digit, non-dot character,
	/// so a `Number` token never carries a trailing `%` itself. `parse_set`'s
	/// `LoopOffset` arm accepts the sign whether or not whitespace separates
	/// it from the number, which is the only variation actually reachable;
	/// its `lit.ends_with('%')` branch, written for a fused `"25%"` token
	/// the lexer does not produce, is dead code kept alive only by this not
	/// yet being noticed.
	#[test]
	fn set_loop_offset_accepts_a_percent_sign_separated_by_whitespace() {
		let Commands::Set(set) = one("Set LoopOffset 25 %") else {
			panic!("expected a Set command");
		};
		assert_eq!(set.setting, Setting::LoopOffset(25.0));
	}

	/// `Set CursorBlink` compares the token's literal text against the exact
	/// strings `"true"`/`"false"`, not against `TokenType::Boolean` — so
	/// casing that the lexer's own keyword table would reject as a `Boolean`
	/// token doesn't matter here at all, only spelling does. `True` reads as
	/// a plain `String` token and then fails this string comparison too.
	#[test]
	fn set_cursor_blink_is_case_sensitive() {
		let (commands, errors) = tape::parse("Set CursorBlink True");
		assert!(!errors.is_empty(), "expected a parse error");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Set(_))));
	}

	/// `WaitPattern`'s value arrives as a plain `String` token, not the
	/// `/pattern/` `Regex` token `Wait` itself uses — a different code path
	/// through the lexer that still has to reach the same
	/// `Regex::new(..).is_err()` guard rather than handing an unparsable
	/// pattern to whatever eventually calls `Regex::new` on it unchecked.
	#[test]
	fn set_wait_pattern_rejects_an_invalid_regex() {
		let (commands, errors) = tape::parse(r#"Set WaitPattern "(""#);
		assert!(!errors.is_empty(), "expected a parse error");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Set(_))));
	}

	// ---- Output / the typed format -----------------------------------------

	#[test]
	fn output_infers_its_format_from_the_extension() {
		let Commands::Output(output) = one("Output out.mp4") else {
			panic!("expected an Output command");
		};
		assert_eq!(output.format, Output::Mp4);
		assert_eq!(output.path, PathBuf::from("out.mp4"));

		let Commands::Output(output) = one("Output frame.png") else {
			panic!("expected an Output command");
		};
		assert_eq!(output.format, Output::Png);

		let Commands::Output(output) = one("Output movie.svg") else {
			panic!("expected an Output command");
		};
		assert_eq!(output.format, Output::Svg);
	}

	/// `Output::from_extension` lower-cases before matching, and `FromStr`
	/// goes through it — a tape naming `OUT.MP4` should agree with the CLI's
	/// own `--output OUT.MP4`, which already normalised case before this.
	#[test]
	fn output_extension_matching_is_case_insensitive() {
		let Commands::Output(output) = one("Output OUT.MP4") else {
			panic!("expected an Output command");
		};
		assert_eq!(output.format, Output::Mp4);
	}

	/// A trailing slash names a folder of still frames rather than one file,
	/// and there is no extension to read a format from — it always means
	/// PNG.
	#[test]
	fn a_trailing_slash_names_a_folder_of_png_stills() {
		let Commands::Output(output) = one("Output stills/") else {
			panic!("expected an Output command");
		};
		assert_eq!(output.format, Output::Png);
		assert_eq!(output.path, PathBuf::from("stills/"));
	}

	#[test]
	fn a_path_with_neither_an_extension_nor_a_trailing_slash_is_rejected() {
		let (commands, errors) = tape::parse("Output stills");
		assert!(!errors.is_empty());
		assert!(!commands.iter().any(|c| matches!(c, Commands::Output(_))));
	}

	/// Before `Output` grew a `FromStr` impl this fell through to `burn` with
	/// a `format` field of `".gif"` and only failed once the PTY was already
	/// open. Rejecting it here, at parse time, is what makes `dvd check`
	/// catch it the same way it catches a missing quote.
	#[test]
	fn an_unsupported_output_extension_is_rejected_at_parse_time() {
		let (commands, errors) = tape::parse("Output out.gif");
		assert!(!errors.is_empty());
		assert!(
			errors[0].message.contains("mp4"),
			"the error should name the allowed extensions: {errors:?}"
		);
		assert!(!commands.iter().any(|c| matches!(c, Commands::Output(_))));
	}

	// ---- Require / Screenshot / Copy / Paste / Env / Hide / Show ----------

	#[test]
	fn require_captures_the_program_name() {
		let Commands::Require(require) = one(r#"Require "ffmpeg""#) else {
			panic!("expected a Require command");
		};
		assert_eq!(require.program, "ffmpeg");
	}

	#[test]
	fn screenshot_requires_a_dot_png_path() {
		let Commands::Screenshot(shot) = one("Screenshot frame.png") else {
			panic!("expected a Screenshot command");
		};
		assert_eq!(shot.path, PathBuf::from("frame.png"));

		let (commands, errors) = tape::parse("Screenshot frame.jpg");
		assert!(!errors.is_empty());
		assert!(
			!commands
				.iter()
				.any(|c| matches!(c, Commands::Screenshot(_)))
		);
	}

	#[test]
	fn copy_and_paste_round_trip_through_the_clipboard_field() {
		let Commands::Copy(copy) = one(r#"Copy "hello""#) else {
			panic!("expected a Copy command");
		};
		assert_eq!(copy.text, "hello");
		assert!(matches!(one("Paste"), Commands::Paste));
	}

	#[test]
	fn env_captures_a_name_and_a_value() {
		let Commands::Env(env) = one(r#"Env API_KEY "secret""#) else {
			panic!("expected an Env command");
		};
		assert_eq!(env.variable, "API_KEY");
		assert_eq!(env.value, "secret");
	}

	#[test]
	fn hide_and_show_carry_no_data() {
		assert!(matches!(one("Hide"), Commands::Hide));
		assert!(matches!(one("Show"), Commands::Show));
	}

	// ---- Comments and layout ------------------------------------------------

	#[test]
	fn blank_lines_between_commands_are_ignored() {
		let commands = parse_ok("Sleep 1s\n\n\nType \"hi\"");
		assert_eq!(commands.len(), 2);
	}

	#[test]
	fn a_comment_can_trail_a_command_on_the_same_line() {
		let commands = parse_ok("Sleep 1s # take a breath\nType \"hi\"");
		assert_eq!(commands.len(), 2);
	}

	// ---- The parse_time panic fix -------------------------------------------

	/// The lexer's `read_number` accepts any run of digits and dots, so it can
	/// hand the parser a `Number` token like `0.0.0.0` that no `f64` can
	/// parse. `parse_time` used to `.unwrap()` that parse; a tape containing
	/// it crashed the whole process instead of failing to parse. This is the
	/// regression test for the fix.
	#[test]
	fn a_malformed_number_reports_an_error_instead_of_panicking() {
		let (commands, errors) = tape::parse("Sleep 0.0.0.0");
		assert!(!errors.is_empty(), "expected a parse error, got none");
		assert!(
			!commands
				.iter()
				.any(|c| matches!(c, Commands::Sleep(s) if s.duration.is_some())),
			"a Sleep with a malformed duration should not report one: {commands:?}"
		);
	}

	/// The `@<time>` prefix reaches `parse_time` through `parse_speed`, a
	/// different call path from `Sleep`'s direct one. This confirms the same
	/// fix protects both instead of just whichever path got tested first.
	#[test]
	fn a_malformed_rate_prefix_also_avoids_a_panic() {
		let (commands, errors) = tape::parse(r#"Type@0.0.0.0s "hi""#);
		assert!(!errors.is_empty(), "expected a parse error, got none");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Type(_))));
	}

	// ---- Error location -------------------------------------------------

	#[test]
	fn a_missing_setting_value_is_reported_at_the_setting_keyword() {
		let (_, errors) = tape::parse("Set FontSize");
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert_eq!(errors[0].token.line, 1);
		assert_eq!(errors[0].token.column, 5);
	}

	/// A space separates `Wait` from its pattern here for the same reason
	/// `wait_accepts_a_trailing_regex_pattern` needs one: glued directly on,
	/// `/(/ ` would lex as part of the `Wait` bare word instead of its own
	/// `Regex` token, and this test is about where the error on the regex
	/// itself gets reported, not about that separate, already-documented
	/// bug.
	#[test]
	fn a_bad_regex_in_wait_is_reported_at_the_regex_token() {
		let (_, errors) = tape::parse("Wait /(/");
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert_eq!(errors[0].token.line, 1);
		assert_eq!(errors[0].token.column, 6);
	}

	/// Line numbers reset on every `\n`, but so must columns — a lexer that
	/// forgot to zero the column would report every mistake after the first
	/// line shifted by whatever came before it. Line three is deliberately
	/// neither the first nor the second line, which is where that kind of
	/// off-by-one hides.
	#[test]
	fn a_mistake_on_the_third_line_is_reported_on_the_third_line() {
		let (_, errors) = tape::parse("Type \"ok\"\nSleep 1s\nSleep 0.0.0.0");
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert_eq!(errors[0].token.line, 3);
		assert_eq!(errors[0].token.column, 7);
	}

	/// Every numeric `Setting` parses through `str::parse::<u32>()` (or
	/// `f32`/`f64`) and propagates its `Err` with `?` rather than unwrapping,
	/// so a value that overflows the target type is a rejected tape, not a
	/// crashed process.
	#[test]
	fn a_setting_value_too_large_for_its_integer_type_is_rejected_not_panicked() {
		let (commands, errors) = tape::parse("Set FontSize 99999999999");
		assert!(!errors.is_empty(), "expected a parse error, got none");
		assert!(!commands.iter().any(|c| matches!(c, Commands::Set(_))));
	}

	/// Unlike a setting's value, a keypress repeat count goes through
	/// `parse_repeat`'s `.unwrap_or(1)` — a value too large for `u32` is
	/// silently treated as though no count had been given at all, rather
	/// than reported as an error. Documented here as a real quirk of the
	/// language, not a gap in this suite: `Up 99999999999999999999` parses
	/// clean and repeats once, which would surprise whoever fat-fingered an
	/// extra few zeroes.
	#[test]
	fn a_repeat_count_too_large_for_u32_silently_falls_back_to_one() {
		let Commands::Key(key) = one("Up 99999999999999999999") else {
			panic!("expected a Key command");
		};
		assert_eq!(key.repeat_count, 1);
	}

	// ---- The malformed-input corpus ------------------------------------

	/// A very deep modifier chain has to stay iterative — `parse_ctrl`'s
	/// `while` loop, not recursion — or an absurdly long but *valid* tape
	/// like this one would blow the stack before it blew a time budget.
	#[test]
	fn a_very_deep_modifier_chain_does_not_overflow_the_stack() {
		let source = format!("Ctrl{}+c", "+Alt".repeat(5_000));
		let (commands, errors) = tape::parse(&source);
		assert!(errors.is_empty(), "{errors:?}");
		assert_eq!(commands.len(), 1);
	}

	/// Every one of these tapes is broken in a different way. None may
	/// panic, and none may leave a command behind that was built from the
	/// broken input — a `Set` with no value must not silently become a
	/// `Set` with a zero, and an unknown command must not become a no-op.
	#[test]
	fn a_corpus_of_malformed_tapes_never_panics_and_never_smuggles_out_a_command() {
		let deep_malformed_chain = format!("Ctrl+c{}", "+Alt".repeat(3_000));

		let corpus: Vec<(&str, &str, bool)> = vec![
			("a truncated Type command", "Type", true),
			("a truncated Ctrl command", "Ctrl", true),
			("a truncated Alt command", "Alt", true),
			("a truncated Env command", "Env", true),
			("a truncated Require command", "Require", true),
			("a truncated Wait+", "Wait+", true),
			("a truncated Output command", "Output", true),
			("an unknown keyword", "Frobnicate", true),
			("a setting with no value", "Set FontSize", true),
			("a bad regex", "Wait/(/", true),
			(
				"an unterminated string where an extension is required",
				"Screenshot \"abc",
				true,
			),
			("garbage bytes", "\u{1f4a5}", true),
			(
				"a modifier appearing after a non-modifier, arbitrarily deep",
				deep_malformed_chain.as_str(),
				true,
			),
			("an empty file", "", false),
			(
				"a file of only comments",
				"# nothing to see here\n# still nothing",
				false,
			),
		];

		for (label, source, expect_error) in corpus {
			let (commands, errors) = tape::parse(source);
			assert_eq!(
				!errors.is_empty(),
				expect_error,
				"{label} ({source:?}): expected error presence {expect_error}, got {errors:?}"
			);
			assert!(
				commands.is_empty(),
				"{label} ({source:?}): expected no commands, got {commands:?}"
			);
		}
	}
}
