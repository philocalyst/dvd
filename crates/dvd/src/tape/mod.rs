//! The `.dvd` tape language: source text in, a command list out.
//!
//! Three stages, each in its own module. [`token`] is the vocabulary,
//! [`lexer`] turns bytes into tokens, [`parser`] turns tokens into
//! [`Commands`]. Nothing here touches a terminal, a PTY, or a renderer — a
//! tape can be validated (`dvd check`) with none of that present, which is
//! the whole reason this stage stands alone.

pub mod lexer;
pub mod parser;
pub mod token;

pub use parser::{Chord, Commands, ParseError, Setting, WaitMode};
pub use token::TokenType;

use lexer::Lexer;
use parser::Parser;

/// Parse a whole tape, returning the commands alongside every error found.
///
/// Errors do not stop the parse: a tape with three mistakes reports all three
/// rather than making the author fix them one run at a time.
pub fn parse(source: &str) -> (Vec<Commands>, Vec<ParseError>) {
	let mut lexer = Lexer::new(source);
	let mut parser = Parser::new(&mut lexer);
	let commands = parser.parse();
	let errors = parser.errors().to_vec();
	(commands, errors)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	/// A smoke test of the whole stage — source text through the lexer and
	/// parser to a command list — where the per-rule detail lives in
	/// `parser::tests`. A tape exercising several commands at once parses clean.
	#[test]
	fn a_representative_tape_parses_end_to_end() {
		let (commands, errors) =
			parse("# a greeting\nSet Theme \"nord\"\nType@100ms \"cd \" \"/tmp\"\nHome\nSleep 1s");
		assert!(errors.is_empty(), "unexpected parse errors: {errors:?}");
		assert!(matches!(
			commands.as_slice(),
			[
				Commands::Set(Setting::Theme(_)),
				Commands::Type { rate: Some(rate), text },
				Commands::Key { key: TokenType::Home, .. },
				Commands::Sleep { duration: Some(_) },
			] if *rate == Duration::from_millis(100) && text == "cd /tmp"
		));
	}
}
