//! Source text to tokens.
//!
//! The cursor is a `CharIndices` rather than a `Chars` plus a counter, and that
//! choice is load-bearing twice over. It keeps `offset` a *byte* offset, so
//! slicing `input` can't land mid-codepoint on a tape containing anything
//! outside ASCII; and it makes "where does the current token end?" answerable
//! at end of input, where a counter that only advances on a successful read
//! stalls one character short and truncates the final token.

use super::token::{Token, TokenType, lookup_identifier};
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

impl<'source> Lexer<'source> {
	/// Create a new lexer over the source input.
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
	///
	/// That last part is the whole point: a token running to end of input ends
	/// at `input.len()`, not at the offset of the last character it consumed.
	fn offset(&self) -> usize {
		self.current.map_or(self.input.len(), |(offset, _)| offset)
	}

	/// The character under the cursor, if any.
	fn peek_current(&self) -> Option<char> {
		self.current.map(|(_, ch)| ch)
	}

	/// Read the next token.
	pub fn next_token(&mut self) -> Token {
		self.skip_whitespace();

		let mut token = Token {
			line: self.line,
			column: self.column,
			..Token::default()
		};

		match self.peek_current() {
			// No character under the cursor: the input is exhausted.
			None => {
				token.token_type = TokenType::Eof;
				token.literal = "\0".to_string();
			}
			Some('@') => {
				token = self.new_token(TokenType::At, '@');
				self.read_char();
			}
			Some('=') => {
				token = self.new_token(TokenType::Equal, '=');
				self.read_char();
			}
			Some(']') => {
				token = self.new_token(TokenType::RightBracket, ']');
				self.read_char();
			}
			Some('[') => {
				token = self.new_token(TokenType::LeftBracket, '[');
				self.read_char();
			}
			Some('-') => {
				token = self.new_token(TokenType::Minus, '-');
				self.read_char();
			}
			Some('%') => {
				token = self.new_token(TokenType::Percent, '%');
				self.read_char();
			}
			Some('^') => {
				token = self.new_token(TokenType::Caret, '^');
				self.read_char();
			}
			Some('\\') => {
				token = self.new_token(TokenType::Backslash, '\\');
				self.read_char();
			}
			Some('#') => {
				token.token_type = TokenType::Comment;
				token.literal = self.read_comment().to_string();
			}
			Some('+') => {
				token = self.new_token(TokenType::Plus, '+');
				self.read_char();
			}
			Some('{') => {
				token.token_type = TokenType::Json;
				// TODO: JSON escaping is not handled; a `}` inside a string
				// value ends the token early.
				token.literal = format!("{{{}}}", self.read_string('}'));
				self.read_char();
			}
			Some(quote @ ('`' | '\'' | '"')) => {
				token.token_type = TokenType::String;
				token.literal = self.read_string(quote).to_string();
				self.read_char();
			}
			Some('/') => {
				token.token_type = TokenType::Regex;
				token.literal = self.read_string('/').to_string();
				self.read_char();
			}
			// Not a punctuation token, so it is a number, a word, or garbage.
			Some(ch) => {
				// A leading `.` counts as a number only when a digit follows,
				// so `.5` lexes as one but a bare `.` stays illegal.
				if ch.is_ascii_digit()
					|| (ch == '.' && self.peek_char().is_some_and(|c| c.is_ascii_digit()))
				{
					token.literal = self.read_number().to_string();
					token.token_type = TokenType::Number;
				} else if ch.is_alphabetic() {
					token.literal = self.read_identifier().to_string();
					token.token_type = lookup_identifier(&token.literal);
				} else {
					token = self.new_token(TokenType::Illegal, ch);
					self.read_char();
				}
			}
		}

		token
	}

	/// Build a single-character token at the current position.
	fn new_token(&self, token_type: TokenType, ch: char) -> Token {
		Token {
			token_type,
			literal: ch.to_string(),
			line: self.line,
			column: self.column,
		}
	}

	/// Consume the opening delimiter, then read up to (not including) the
	/// closing one. A newline ends the run too, so an unterminated string stops
	/// at the end of its line instead of swallowing the rest of the tape.
	fn read_string(&mut self, end_char: char) -> &'source str {
		self.read_char();
		self.read_delimited(|ch| ch == end_char || ch == '\n' || ch == '\r')
	}

	/// Consume the `#` and return the rest of the line, marker excluded.
	fn read_comment(&mut self) -> &'source str {
		self.read_char();
		self.read_delimited(|ch| ch == '\n' || ch == '\r')
	}

	/// Read from the cursor until `is_end` matches or the input runs out. The
	/// terminator is left under the cursor for the caller to consume.
	fn read_delimited(&mut self, is_end: impl Fn(char) -> bool) -> &'source str {
		// `input` is a shared reference and therefore `Copy`, so the slice
		// borrows the source rather than `self` — leaving the cursor free to
		// advance while a previous token's text is still held.
		let input = self.input;

		if self.peek_current().is_some_and(&is_end) {
			let empty = self.offset();
			return &input[empty..empty];
		}

		let start = self.offset();
		while let Some(ch) = self.peek_current() {
			if is_end(ch) {
				break;
			}
			self.read_char();
		}
		&input[start..self.offset()]
	}

	fn read_number(&mut self) -> &'source str {
		let input = self.input;
		let start = self.offset();
		// TODO: `0.0.0.0` lexes as one number here and fails later at `parse`.
		while self
			.peek_current()
			.is_some_and(|ch| ch.is_ascii_digit() || ch == '.')
		{
			self.read_char();
		}
		&input[start..self.offset()]
	}

	fn read_identifier(&mut self) -> &'source str {
		let input = self.input;
		let start = self.offset();
		while self.peek_current().is_some_and(|ch| {
			ch.is_alphanumeric() || ch == '.' || ch == '-' || ch == '_' || ch == '/' || ch == '%'
		}) {
			self.read_char();
		}
		&input[start..self.offset()]
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

#[cfg(test)]
mod tests {
	use super::*;

	fn tokens(source: &str) -> Vec<(TokenType, String)> {
		let mut lexer = Lexer::new(source);
		let mut out = Vec::new();
		loop {
			let token = lexer.next_token();
			if token.token_type == TokenType::Eof {
				return out;
			}
			out.push((token.token_type, token.literal));
		}
	}

	/// The bug that made `Home` lex as `Hom`: a token ending at end of input
	/// used to lose its last character.
	#[test]
	fn a_word_ending_at_end_of_input_keeps_its_last_character() {
		assert_eq!(tokens("Home"), vec![(TokenType::Home, "Home".into())]);
	}

	#[test]
	fn a_unit_suffix_ending_at_end_of_input_survives() {
		assert_eq!(
			tokens("Sleep 1s"),
			vec![
				(TokenType::Sleep, "Sleep".into()),
				(TokenType::Number, "1".into()),
				(TokenType::Seconds, "s".into()),
			]
		);
	}

	/// `position` used to be a character count used to slice bytes, so any
	/// multi-byte character shifted every later token — or panicked outright.
	#[test]
	fn multibyte_text_slices_on_character_boundaries() {
		assert_eq!(
			tokens(r#"Type "héllo → wörld""#),
			vec![
				(TokenType::Type, "Type".into()),
				(TokenType::String, "héllo → wörld".into()),
			]
		);
	}

	#[test]
	fn an_empty_string_literal_lexes_as_empty() {
		assert_eq!(
			tokens(r#"Type """#),
			vec![
				(TokenType::Type, "Type".into()),
				(TokenType::String, String::new()),
			]
		);
	}

	#[test]
	fn a_comment_runs_to_end_of_line_only() {
		assert_eq!(
			tokens("# note\nSleep"),
			vec![
				(TokenType::Comment, " note".into()),
				(TokenType::Sleep, "Sleep".into()),
			]
		);
	}

	#[test]
	fn a_comment_at_end_of_input_terminates() {
		assert_eq!(tokens("# note"), vec![(TokenType::Comment, " note".into())]);
	}

	#[test]
	fn line_numbers_follow_newlines() {
		let mut lexer = Lexer::new("Sleep\nType");
		assert_eq!(lexer.next_token().line, 1);
		assert_eq!(lexer.next_token().line, 2);
	}

	/// `read_number` accepts any run of digits and dots, with no check that
	/// what it collects is a single valid number. `parser::parse_time` has to
	/// cope with the result — this is the token it copes with, pinned down
	/// so a future change to either side notices the other.
	#[test]
	fn a_number_with_several_dots_lexes_as_one_token_the_parser_must_reject() {
		assert_eq!(
			tokens("0.0.0.0"),
			vec![(TokenType::Number, "0.0.0.0".into())]
		);
	}

	/// There is no escape mechanism: `read_string` stops at the first quote,
	/// newline, or end of input, full stop. A backslash is an ordinary
	/// character, so `\"` does not protect the quote that follows it — the
	/// string ends there, the `b` after it lexes as its own bare word, and
	/// the stray closing quote left over opens a third, empty, unterminated
	/// string rather than pairing back up with anything.
	#[test]
	fn a_backslash_does_not_escape_the_closing_quote() {
		assert_eq!(
			tokens(r#""a\"b""#),
			vec![
				(TokenType::String, "a\\".into()),
				(TokenType::String, "b".into()),
				(TokenType::String, String::new()),
			]
		);
	}

	/// A string with no closing quote does not stall the lexer or lose text —
	/// it simply reads to the end of the input, the same rule that already
	/// stops it at a newline.
	#[test]
	fn an_unterminated_string_reads_to_end_of_input() {
		assert_eq!(tokens(r#""abc"#), vec![(TokenType::String, "abc".into())]);
	}

	/// `@` and `+` are always single-character punctuation tokens: nothing
	/// about being adjacent to a number, a unit, or each other merges them
	/// into a compound token. This is what lets `parse_ctrl` walk `@10ms+c`
	/// one token at a time without a special case for the boundary.
	#[test]
	fn at_and_plus_never_merge_with_their_neighbours() {
		assert_eq!(
			tokens("@10ms+c"),
			vec![
				(TokenType::At, "@".into()),
				(TokenType::Number, "10".into()),
				(TokenType::Milliseconds, "ms".into()),
				(TokenType::Plus, "+".into()),
				(TokenType::String, "c".into()),
			]
		);
	}

	/// `lookup_identifier` is an exact match against `KEYWORDS`, not a
	/// prefix or case-insensitive one. This is the whole reason an unquoted
	/// path like `out.mp4` or an unquoted theme name like `nord` lexes as a
	/// plain `String` — any word that is not *exactly* one of the reserved
	/// spellings falls through to that arm instead of erroring.
	#[test]
	fn a_bare_word_is_a_string_unless_it_matches_a_keyword_exactly() {
		assert_eq!(tokens("Sleep"), vec![(TokenType::Sleep, "Sleep".into())]);
		// A keyword-looking prefix that isn't the whole word.
		assert_eq!(tokens("Sleepy"), vec![(TokenType::String, "Sleepy".into())]);
		// Case matters: only the lowercase spelling is registered.
		assert_eq!(tokens("SLEEP"), vec![(TokenType::String, "SLEEP".into())]);
		// An ordinary unquoted path, which is how `Output out.mp4` works.
		assert_eq!(
			tokens("out.mp4"),
			vec![(TokenType::String, "out.mp4".into())]
		);
	}
}
