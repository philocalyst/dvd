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

pub use parser::{
	Commands, CopyCommand, CtrlCommand, EnvCommand, KeyCommand, OutputCommand, ParseError,
	RequireCommand, ScreenshotCommand, Setting, SleepCommand, TypeCommand, WaitCommand, WaitMode,
};
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

	fn commands(source: &str) -> Vec<Commands> {
		let (commands, errors) = parse(source);
		assert!(errors.is_empty(), "unexpected parse errors: {errors:?}");
		commands
	}

	#[test]
	fn adjacent_string_literals_concatenate() {
		let parsed = commands(r#"Type "cd " "/tmp""#);
		let [Commands::Type(typed)] = &parsed[..] else {
			panic!("expected one Type command, got {parsed:?}");
		};
		assert_eq!(typed.text, "cd /tmp");
	}

	#[test]
	fn home_is_a_reachable_key() {
		let parsed = commands("Home");
		let [Commands::Key(key)] = &parsed[..] else {
			panic!("expected one Key command, got {parsed:?}");
		};
		assert_eq!(key.key, TokenType::Home);
	}

	/// The parser does not resync after an error, so one bad line cascades into
	/// several reports. What matters is that it refuses rather than quietly
	/// exporting a variable named `Set` — not the exact error count.
	#[test]
	fn env_rejects_a_keyword_as_a_variable_name() {
		let (commands, errors) = parse(r#"Env Set "value""#);
		assert!(!errors.is_empty());
		assert!(
			!commands.iter().any(|c| matches!(c, Commands::Env(_))),
			"no Env command should survive: {commands:?}"
		);
	}

	#[test]
	fn a_rate_prefix_becomes_a_duration() {
		let parsed = commands(r#"Type@100ms "hi""#);
		let [Commands::Type(typed)] = &parsed[..] else {
			panic!("expected one Type command, got {parsed:?}");
		};
		assert_eq!(typed.rate, Some(Duration::from_millis(100)));
	}

	#[test]
	fn comments_are_skipped_without_error() {
		assert!(commands("# just a note\nSleep 1s").len() == 1);
	}
}
