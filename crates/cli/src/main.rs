use clap::Parser;

/// snip: copy and paste code between machines through the clipboard.
#[derive(Parser)]
#[command(name = "snip", version)]
struct Cli {}

fn main() {
	Cli::parse();
	println!("snip {}", env!("CARGO_PKG_VERSION"));
}
