//! The vocabulary of the tape language: one [`TokenType`] per lexeme.
//!
//! Keyword spellings live on the enum itself. `strum`'s `EnumString` derives the
//! identifier lookup from each variant's name, so the keyword table is the type
//! definition — there is no second list to keep in step with it. The variants
//! that are punctuation, literals or sentinels are `disabled`: the lexer emits
//! those directly and they must never be reachable by looking up a bare word.

use std::str::FromStr;
use strum::EnumString;

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

#[derive(Debug, Clone, PartialEq, Eq, Default, EnumString)]
pub enum TokenType {
	// Punctuation and sentinels: produced by the lexer, never by keyword lookup.
	#[strum(disabled)]
	At,
	#[strum(disabled)]
	Equal,
	#[strum(disabled)]
	Plus,
	#[strum(disabled)]
	Percent,
	#[strum(disabled)]
	Backslash,
	#[strum(disabled)]
	Minus,
	#[strum(disabled)]
	RightBracket,
	#[strum(disabled)]
	LeftBracket,
	#[strum(disabled)]
	Caret,
	#[strum(disabled)]
	Eof,
	#[default]
	#[strum(disabled)]
	Illegal,
	#[strum(disabled)]
	Comment,
	#[strum(disabled)]
	Number,
	#[strum(disabled)]
	String,
	#[strum(disabled)]
	Json,
	#[strum(disabled)]
	Regex,

	// Time and size units — lower-case, so they carry an explicit spelling.
	#[strum(serialize = "em")]
	Em,
	#[strum(serialize = "px")]
	Px,
	#[strum(serialize = "ms")]
	Milliseconds,
	#[strum(serialize = "s")]
	Seconds,
	#[strum(serialize = "m")]
	Minutes,

	// Either boolean spelling lexes to the one token.
	#[strum(serialize = "true", serialize = "false")]
	Boolean,

	// Keys, movement, commands and settings — spelled exactly as the variant.
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
	Down,
	Left,
	Right,
	Up,
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

impl TokenType {
	/// Resolve an identifier to its keyword, or [`TokenType::String`] when it is
	/// none — the fall-through that lets an unquoted path like `out.mp4` or a
	/// bare theme name like `nord` lex as a plain string rather than an error.
	/// The match is exact and case-sensitive: `Sleepy` and `SLEEP` are strings.
	pub fn lookup(identifier: &str) -> TokenType {
		TokenType::from_str(identifier).unwrap_or(TokenType::String)
	}

	/// The tokens that may follow `Set`.
	pub fn is_setting(&self) -> bool {
		use TokenType::*;
		matches!(
			self,
			Shell
				| FontFamily | FontSize
				| LetterSpacing
				| LineHeight | Framerate
				| TypingSpeed
				| Theme | PlaybackSpeed
				| Height | Width
				| Padding | LoopOffset
				| MarginFill | Margin
				| WindowBar | WindowBarSize
				| BorderRadius
				| CursorBlink
				| WaitTimeout
				| WaitPattern
		)
	}

	/// The modifiers that may lead a `Ctrl` chord (`Ctrl+Alt+…`, `Ctrl+Shift+…`).
	pub fn is_modifier(&self) -> bool {
		matches!(self, TokenType::Alt | TokenType::Shift)
	}
}
