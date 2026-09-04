//! Source text to tokens.
//!
//! The cursor is a `CharIndices` rather than a `Chars` plus a counter, and that
//! choice is load-bearing twice over. It keeps `offset` a *byte* offset, so
//! slicing `input` can't land mid-codepoint on a tape containing anything
//! outside ASCII; and it makes "where does the current token end?" answerable
//! at end of input, where a counter that only advances on a successful read
//! stalls one character short and truncates the final token.

use super::token::{Token, TokenType};
use std::iter::Peekable;
use std::str::CharIndices;

pub struct Lexer<'source> {
	input: &'source str,
	chars: Peekable<CharIndices<'source>>,
	/// Byte offset and value of the character under the cursor. `None` once the
	/// input is exhausted.
	current: Option<(usize, char)>,
	line: usize,
	column: usize,
}

/// The one-character punctuation tokens, each of which stands alone regardless
/// of its neighbours — so `parse_ctrl` can walk `@10ms+c` a token at a time.
fn punctuation(ch: char) -> Option<TokenType> {
	Some(match ch {
		'@' => TokenType::At,
		'=' => TokenType::Equal,
		'+' => TokenType::Plus,
		'%' => TokenType::Percent,
		'^' => TokenType::Caret,
		'\\' => TokenType::Backslash,
		'-' => TokenType::Minus,
		'[' => TokenType::LeftBracket,
		']' => TokenType::RightBracket,
		_ => return None,
	})
}

impl<'source> Lexer<'source> {
	pub fn new(input: &'source str) -> Self {
		let mut lexer = Lexer {
			input,
			chars: input.char_indices().peekable(),
			current: None,
			line: 1,
			column: 0,
		};
		lexer.read_char();
		lexer
	}

	/// Consume the character under the cursor and advance.
	fn read_char(&mut self) {
		self.column += 1;
		self.current = self.chars.next();
	}

	/// Peek one character ahead without consuming.
	fn peek_char(&mut self) -> Option<char> {
		self.chars.peek().map(|&(_, ch)| ch)
	}

	/// Byte offset of the cursor — the end of the input once it is exhausted.
	/// That last part is the whole point: a token running to end of input ends
	/// at `input.len()`, not at the offset of the last character it consumed.
	fn offset(&self) -> usize {
		self.current.map_or(self.input.len(), |(offset, _)| offset)
	}

	/// The character under the cursor, if any.
	fn peek_current(&self) -> Option<char> {
		self.current.map(|(_, ch)| ch)
	}

	pub fn next_token(&mut self) -> Token {
		self.skip_whitespace();
		let (line, column) = (self.line, self.column);
		let make = |token_type, literal: String| Token {
			token_type,
			literal,
			line,
			column,
		};

		let Some(ch) = self.peek_current() else {
			return make(TokenType::Eof, "\0".to_string());
		};

		if let Some(token_type) = punctuation(ch) {
			self.read_char();
			make(token_type, ch.to_string())
		} else if ch == '#' {
			make(TokenType::Comment, self.read_delimited_after(is_newline))
		} else if ch == '{' {
			// TODO: JSON escaping is not handled; a `}` inside a string value
			// ends the token early.
			let body = self.read_delimited_after(|c| c == '}' || is_newline(c));
			self.read_char();
			make(TokenType::Json, format!("{{{body}}}"))
		} else if matches!(ch, '`' | '\'' | '"') {
			let literal = self.read_delimited_after(|c| c == ch || is_newline(c));
			self.read_char();
			make(TokenType::String, literal)
		} else if ch == '/' {
			let literal = self.read_delimited_after(|c| c == '/' || is_newline(c));
			self.read_char();
			make(TokenType::Regex, literal)
		} else if ch.is_ascii_digit()
			|| (ch == '.' && self.peek_char().is_some_and(|c| c.is_ascii_digit()))
		{
			// A leading `.` counts as a number only when a digit follows, so `.5`
			// lexes as one but a bare `.` stays illegal. `read_number` accepts any
			// run of digits and dots — `0.0.0.0` lexes here and fails at `parse`.
			make(
				TokenType::Number,
				self.read_while(|c| c.is_ascii_digit() || c == '.'),
			)
		} else if ch.is_alphabetic() {
			let word = self.read_while(is_identifier);
			make(TokenType::lookup(&word), word)
		} else {
			self.read_char();
			make(TokenType::Illegal, ch.to_string())
		}
	}

	/// Consume the opening delimiter, then read up to (not including) the first
	/// character `is_end` accepts, leaving it under the cursor for the caller.
	/// A newline is an accepted terminator for strings and comments, so an
	/// unterminated one stops at its line rather than swallowing the rest.
	fn read_delimited_after(&mut self, is_end: impl Fn(char) -> bool) -> String {
		self.read_char();
		self.read_while(|c| !is_end(c))
	}

	/// Read from the cursor while `keep` holds, returning the borrowed slice as
	/// an owned `String`. `input` is `Copy`, so the slice borrows the source
	/// rather than `self`, leaving the cursor free to advance.
	fn read_while(&mut self, keep: impl Fn(char) -> bool) -> String {
		let input = self.input;
		let start = self.offset();
		while self.peek_current().is_some_and(&keep) {
			self.read_char();
		}
		input[start..self.offset()].to_string()
	}

	fn skip_whitespace(&mut self) {
		while let Some(ch) = self.peek_current() {
			if !ch.is_whitespace() {
				break;
			}
			if ch == '\n' {
				self.line += 1;
				self.column = 0;
			}
			self.read_char();
		}
	}
}

fn is_newline(ch: char) -> bool {
	ch == '\n' || ch == '\r'
}

fn is_identifier(ch: char) -> bool {
	ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_' | '/' | '%')
}

#[cfg(test)]
mod tests {
	use super::*;
	use rstest::rstest;

	fn tokens(source: &str) -> Vec<(TokenType, String)> {
		let mut lexer = Lexer::new(source);
		std::iter::from_fn(|| {
			let token = lexer.next_token();
			(token.token_type != TokenType::Eof).then_some((token.token_type, token.literal))
		})
		.collect()
	}

	fn kinds(source: &str) -> Vec<TokenType> {
		tokens(source).into_iter().map(|(kind, _)| kind).collect()
	}

	/// A pairing helper that keeps the case tables terse without glob-importing
	/// the variants (one of which, `String`, would shadow the std type).
	fn pair(kind: TokenType, literal: &str) -> (TokenType, String) {
		(kind, literal.to_string())
	}

	use TokenType::{
		At, Comment, Milliseconds, Number, Plus, Seconds, Sleep as SleepKw, String as Str, Type,
	};

	#[rstest]
	// A word ending at end of input keeps its last character — the bug that
	// once made `Home` lex as `Hom`.
	#[case("Home", vec![pair(TokenType::Home, "Home")])]
	// A unit suffix at end of input survives the same way.
	#[case("Sleep 1s", vec![pair(SleepKw, "Sleep"), pair(Number, "1"), pair(Seconds, "s")])]
	// `position` was once a character count used to slice bytes, so any
	// multi-byte character shifted every later token — or panicked outright.
	#[case(r#"Type "héllo → wörld""#, vec![pair(Type, "Type"), pair(Str, "héllo → wörld")])]
	#[case(r#"Type """#, vec![pair(Type, "Type"), pair(Str, "")])]
	// A comment runs to end of line only, then lexing resumes.
	#[case("# note\nSleep", vec![pair(Comment, " note"), pair(SleepKw, "Sleep")])]
	#[case("# note", vec![pair(Comment, " note")])]
	// `@` and `+` never merge with a neighbouring number or unit.
	#[case("@10ms+c", vec![pair(At, "@"), pair(Number, "10"), pair(Milliseconds, "ms"), pair(Plus, "+"), pair(Str, "c")])]
	// `read_number` accepts any run of digits and dots with no validity check;
	// `parser::parse_time` copes with the result.
	#[case("0.0.0.0", vec![pair(Number, "0.0.0.0")])]
	// There is no escape mechanism: `\"` does not protect the quote after it, so
	// the string ends at the backslash, `b` becomes a bare word, and the leftover
	// quote opens a third, empty, unterminated string.
	#[case(r#""a\"b""#, vec![pair(Str, "a\\"), pair(Str, "b"), pair(Str, "")])]
	// An unterminated string reads to end of input rather than stalling.
	#[case(r#""abc"#, vec![pair(Str, "abc")])]
	fn lexes_to(#[case] source: &str, #[case] expected: Vec<(TokenType, String)>) {
		assert_eq!(tokens(source), expected);
	}

	#[test]
	fn line_numbers_follow_newlines() {
		let mut lexer = Lexer::new("Sleep\nType");
		assert_eq!(lexer.next_token().line, 1);
		assert_eq!(lexer.next_token().line, 2);
	}

	/// `lookup` is an exact, case-sensitive match, which is the whole reason a
	/// keyword-shaped word that is not *exactly* reserved falls through to a
	/// plain `String` — the mechanism behind unquoted paths and theme names.
	#[rstest]
	#[case("Sleep", SleepKw)]
	#[case("Sleepy", Str)]
	#[case("SLEEP", Str)]
	#[case("out.mp4", Str)]
	fn a_bare_word_is_a_keyword_only_on_an_exact_match(
		#[case] source: &str,
		#[case] kind: TokenType,
	) {
		assert_eq!(kinds(source), vec![kind]);
	}
}
