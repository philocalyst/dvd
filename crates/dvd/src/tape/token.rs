// src/token.rs
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
	pub token_type: TokenType,
	pub literal: String,
	pub line: usize,
	pub column: usize,
}

impl Default for Token {
	fn default() -> Self {
		Token {
			token_type: TokenType::Illegal,
			literal: String::new(),
			line: 1,
			column: 1,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TokenType {
	// Operators
	At,
	Equal,
	Plus,
	Percent,
	Slash,
	Backslash,
	Dot,
	Dash,
	Minus,
	RightBracket,
	LeftBracket,
	Caret,

	// Time units
	Em,
	Milliseconds,
	Minutes,
	Px,
	Seconds,

	// Special
	Eof,
	#[default]
	Illegal,

	// Keys
	Alt,
	Backspace,
	Ctrl,
	Delete,
	End,
	Enter,
	Escape,
	Home,
	Insert,
	PageDown,
	PageUp,
	Sleep,
	Space,
	Tab,
	Shift,

	// Literals
	Comment,
	Number,
	String,
	Json,
	Regex,
	Boolean,

	// Movement
	Down,
	Left,
	Right,
	Up,

	// Commands
	Hide,
	Output,
	Require,
	Set,
	Show,
	Type,
	Screenshot,
	Copy,
	Paste,
	Shell,
	Env,

	// Settings
	FontFamily,
	FontSize,
	Framerate,
	PlaybackSpeed,
	Height,
	Width,
	LetterSpacing,
	LineHeight,
	TypingSpeed,
	Padding,
	Theme,
	LoopOffset,
	MarginFill,
	Margin,
	WindowBar,
	WindowBarSize,
	BorderRadius,
	Wait,
	WaitTimeout,
	WaitPattern,
	CursorBlink,
}

impl fmt::Display for TokenType {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let s = match self {
			TokenType::At => "@",
			TokenType::Equal => "=",
			TokenType::Plus => "+",
			TokenType::Percent => "%",
			TokenType::Slash => "/",
			TokenType::Backslash => "\\",
			TokenType::Dot => ".",
			TokenType::Dash => "-",
			TokenType::Minus => "-",
			TokenType::RightBracket => "]",
			TokenType::LeftBracket => "[",
			TokenType::Caret => "^",
			_ => return write!(f, "{}", to_camel(&format!("{:?}", self))),
		};
		write!(f, "{}", s)
	}
}

use std::borrow::Cow;
use std::sync::LazyLock;
pub static KEYWORDS: LazyLock<HashMap<Cow<'static, str>, TokenType>> = LazyLock::new(|| {
	let mut m = HashMap::new();
	m.insert(Cow::Borrowed("em"), TokenType::Em);
	m.insert(Cow::Borrowed("px"), TokenType::Px);
	m.insert(Cow::Borrowed("ms"), TokenType::Milliseconds);
	m.insert(Cow::Borrowed("s"), TokenType::Seconds);
	m.insert(Cow::Borrowed("m"), TokenType::Minutes);
	m.insert(Cow::Borrowed("Set"), TokenType::Set);
	m.insert(Cow::Borrowed("Sleep"), TokenType::Sleep);
	m.insert(Cow::Borrowed("Type"), TokenType::Type);
	m.insert(Cow::Borrowed("Enter"), TokenType::Enter);
	m.insert(Cow::Borrowed("Space"), TokenType::Space);
	m.insert(Cow::Borrowed("Backspace"), TokenType::Backspace);
	m.insert(Cow::Borrowed("Delete"), TokenType::Delete);
	m.insert(Cow::Borrowed("Insert"), TokenType::Insert);
	m.insert(Cow::Borrowed("Ctrl"), TokenType::Ctrl);
	m.insert(Cow::Borrowed("Alt"), TokenType::Alt);
	m.insert(Cow::Borrowed("Shift"), TokenType::Shift);
	m.insert(Cow::Borrowed("Down"), TokenType::Down);
	m.insert(Cow::Borrowed("Left"), TokenType::Left);
	m.insert(Cow::Borrowed("Right"), TokenType::Right);
	m.insert(Cow::Borrowed("Up"), TokenType::Up);
	m.insert(Cow::Borrowed("PageUp"), TokenType::PageUp);
	m.insert(Cow::Borrowed("PageDown"), TokenType::PageDown);
	m.insert(Cow::Borrowed("Tab"), TokenType::Tab);
	m.insert(Cow::Borrowed("Escape"), TokenType::Escape);
	m.insert(Cow::Borrowed("End"), TokenType::End);
	m.insert(Cow::Borrowed("Home"), TokenType::Home);
	m.insert(Cow::Borrowed("Hide"), TokenType::Hide);
	m.insert(Cow::Borrowed("Require"), TokenType::Require);
	m.insert(Cow::Borrowed("Show"), TokenType::Show);
	m.insert(Cow::Borrowed("Output"), TokenType::Output);
	m.insert(Cow::Borrowed("Shell"), TokenType::Shell);
	m.insert(Cow::Borrowed("FontFamily"), TokenType::FontFamily);
	m.insert(Cow::Borrowed("MarginFill"), TokenType::MarginFill);
	m.insert(Cow::Borrowed("Margin"), TokenType::Margin);
	m.insert(Cow::Borrowed("WindowBar"), TokenType::WindowBar);
	m.insert(Cow::Borrowed("WindowBarSize"), TokenType::WindowBarSize);
	m.insert(Cow::Borrowed("BorderRadius"), TokenType::BorderRadius);
	m.insert(Cow::Borrowed("FontSize"), TokenType::FontSize);
	m.insert(Cow::Borrowed("Framerate"), TokenType::Framerate);
	m.insert(Cow::Borrowed("Height"), TokenType::Height);
	m.insert(Cow::Borrowed("LetterSpacing"), TokenType::LetterSpacing);
	m.insert(Cow::Borrowed("LineHeight"), TokenType::LineHeight);
	m.insert(Cow::Borrowed("PlaybackSpeed"), TokenType::PlaybackSpeed);
	m.insert(Cow::Borrowed("TypingSpeed"), TokenType::TypingSpeed);
	m.insert(Cow::Borrowed("Padding"), TokenType::Padding);
	m.insert(Cow::Borrowed("Theme"), TokenType::Theme);
	m.insert(Cow::Borrowed("Width"), TokenType::Width);
	m.insert(Cow::Borrowed("LoopOffset"), TokenType::LoopOffset);
	m.insert(Cow::Borrowed("WaitTimeout"), TokenType::WaitTimeout);
	m.insert(Cow::Borrowed("WaitPattern"), TokenType::WaitPattern);
	m.insert(Cow::Borrowed("Wait"), TokenType::Wait);
	m.insert(Cow::Borrowed("CursorBlink"), TokenType::CursorBlink);
	m.insert(Cow::Borrowed("true"), TokenType::Boolean);
	m.insert(Cow::Borrowed("false"), TokenType::Boolean);
	m.insert(Cow::Borrowed("Screenshot"), TokenType::Screenshot);
	m.insert(Cow::Borrowed("Copy"), TokenType::Copy);
	m.insert(Cow::Borrowed("Paste"), TokenType::Paste);
	m.insert(Cow::Borrowed("Env"), TokenType::Env);
	m
});

pub fn is_setting(token_type: &TokenType) -> bool {
	matches!(
		token_type,
		TokenType::Shell
			| TokenType::FontFamily
			| TokenType::FontSize
			| TokenType::LetterSpacing
			| TokenType::LineHeight
			| TokenType::Framerate
			| TokenType::TypingSpeed
			| TokenType::Theme
			| TokenType::PlaybackSpeed
			| TokenType::Height
			| TokenType::Width
			| TokenType::Padding
			| TokenType::LoopOffset
			| TokenType::MarginFill
			| TokenType::Margin
			| TokenType::WindowBar
			| TokenType::WindowBarSize
			| TokenType::BorderRadius
			| TokenType::CursorBlink
			| TokenType::WaitTimeout
			| TokenType::WaitPattern
	)
}

pub fn is_command(token_type: &TokenType) -> bool {
	matches!(
		token_type,
		TokenType::Type
			| TokenType::Sleep
			| TokenType::Up
			| TokenType::Down
			| TokenType::Right
			| TokenType::Left
			| TokenType::PageUp
			| TokenType::PageDown
			| TokenType::Enter
			| TokenType::Backspace
			| TokenType::Delete
			| TokenType::Tab
			| TokenType::Escape
			| TokenType::Home
			| TokenType::Insert
			| TokenType::End
			| TokenType::Ctrl
			| TokenType::Screenshot
			| TokenType::Copy
			| TokenType::Paste
			| TokenType::Wait
	)
}

pub fn is_modifier(token_type: &TokenType) -> bool {
	matches!(token_type, TokenType::Alt | TokenType::Shift)
}

pub fn to_camel(s: &str) -> String {
	let parts: Vec<&str> = s.split('_').collect();
	parts
		.iter()
		.map(|p| {
			let mut chars = p.chars();
			match chars.next() {
				None => String::new(),
				Some(first) => {
					first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
				}
			}
		})
		.collect::<Vec<String>>()
		.join("")
}

pub fn lookup_identifier(identifier: &str) -> TokenType {
	KEYWORDS
		.get(identifier)
		.cloned()
		.unwrap_or(TokenType::String)
}
