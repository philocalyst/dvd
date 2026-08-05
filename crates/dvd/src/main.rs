use clap::Parser;

fn main() -> std::process::ExitCode {
	let cli = dvd::cli::Cli::parse();
	dvd::run(cli)
}
