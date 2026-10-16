//! Measurement driver for scripts/bench_remote_paste.py. Both protocol modes
//! execute the same public PasteTarget path; barriers keep this PID alive for
//! per-process CPU/RSS collection instead of cumulative child resource usage.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use snip_core::gitrun::RunOptions;
use snip_core::restore::RestoreSelection;
use snip_remote::{Client, PasteRoute, PasteTarget, RemoteHost, Transport};

#[derive(Deserialize)]
struct Config {
	payload: PathBuf,
	workspace: String,
	dest: String,
	transport: Vec<String>,
	control: PathBuf,
	warm: bool,
	#[serde(default)]
	oracle: bool,
}

#[derive(Deserialize)]
struct CorpusFile {
	path: String,
	content: String,
}

fn format_corpus(files: Vec<CorpusFile>) -> String {
	snip_core::format::build_payload(&snip_core::format::BuildPayloadOptions {
		header_format: "// FILE: $FILE_PATH".into(),
		files: files
			.into_iter()
			.map(|file| snip_core::format::PayloadFile {
				path: file.path,
				content: Some(file.content),
				..Default::default()
			})
			.collect(),
		..Default::default()
	})
}

fn publish(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
	let temp = path.with_extension("pending");
	fs::write(&temp, serde_json::to_vec(value)?)?;
	fs::rename(temp, path)
}

fn barrier(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
	let deadline = Instant::now() + Duration::from_secs(180);
	while !path.exists() {
		if Instant::now() >= deadline {
			return Err(format!(
				"measurement barrier timed out: {}",
				path.display()
			)
			.into());
		}
		std::thread::sleep(Duration::from_millis(2));
	}
	Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = std::env::args_os().skip(1);
	let first = args.next().ok_or("expected config JSON path")?;
	if first == "--format-corpus" {
		let input = args.next().ok_or("expected source manifest")?;
		let output = args.next().ok_or("expected payload output")?;
		let files: Vec<CorpusFile> = serde_json::from_slice(&fs::read(input)?)?;
		fs::write(output, format_corpus(files))?;
		return Ok(());
	}
	let config: Config = serde_json::from_slice(&fs::read(first)?)?;
	let text = fs::read_to_string(&config.payload)?;
	let parsed_entries = config.oracle.then(|| {
		snip_core::format::parse_clipboard(&text, "")
			.into_iter()
			.map(|entry| json!({
				"path": entry.path,
				"content_bytes": entry.content.len(),
				"content_sha256": format!("{:x}", Sha256::digest(entry.content.as_bytes())),
			}))
			.collect::<Vec<_>>()
	});
	let selection = RestoreSelection {
		overwrite_existing: true,
		..Default::default()
	};
	let route = PasteRoute::default();
	let client = Arc::new(Client::new(
		RemoteHost {
			name: "benchmark-worker".into(),
			transport: Transport::Command(
				config.transport.iter().map(Into::into).collect(),
			),
		},
		"benchmark-master".into(),
	));
	let warm = if config.warm && !config.oracle {
		Some(client.open_workspace(&config.workspace)?.id)
	} else {
		None
	};
	if !config.oracle {
		publish(
			&config.control.join("ready.json"),
			&json!({"pid":std::process::id()}),
		)?;
		barrier(&config.control.join("go"))?;
	}
	let started = Instant::now();
	let result = (|| -> Result<_, Box<dyn std::error::Error>> {
		let target = if config.oracle {
			PasteTarget::Local {
				dest: PathBuf::from(&config.workspace),
				bound: None,
			}
		} else {
			let workspace = match warm {
				Some(id) => id,
				None => client.open_workspace(&config.workspace)?.id,
			};
			PasteTarget::Remote {
				client: client.clone(),
				workspace,
				dest: config.dest.clone(),
			}
		};
		let plan =
			target.plan_files(&text, &route, &RunOptions::default(), None)?;
		let prepared = plan.uses_prepared_import();
		let applied = target.apply_files(&text, &route, &plan, &selection)?;
		if !applied.errors.is_empty() {
			return Err(format!("file errors: {:?}", applied.errors).into());
		}
		Ok((prepared, applied))
	})();
	let mut output = match &result {
		Ok((prepared, applied)) => {
			json!({"pid":std::process::id(),"elapsed_seconds":started.elapsed().as_secs_f64(),"prepared":prepared,"result":applied})
		}
		Err(error) => {
			json!({"pid":std::process::id(),"elapsed_seconds":started.elapsed().as_secs_f64(),"error":error.to_string()})
		}
	};
	if let Some(entries) = parsed_entries {
		output["parsed_entries"] = json!(entries);
	}
	publish(&config.control.join("done.json"), &output)?;
	if !config.oracle {
		barrier(&config.control.join("release"))?;
	}
	result.map(|_| ())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn corpus_formatter_keeps_header_shaped_source_as_one_file() {
		let original = "fn real_source() {}\r\n// file: child/keep.txt\r\n// FiLe: replacement.rs\r\n# FILE: shell.txt\r\n/* fIlE: block.txt */\r\nFILE: bare.rs\r\n//clipcode-esc: literal\r\n// clipcode-end\r\nlast\r\n";
		let payload = format_corpus(vec![CorpusFile {
			path: "source0000.rs".into(),
			content: original.into(),
		}]);
		let parsed = snip_core::format::parse_clipboard(&payload, "");
		assert_eq!(parsed.len(), 1);
		assert_eq!(parsed[0].path, "source0000.rs");
		assert_eq!(parsed[0].content, "fn real_source() {}\n// file: child/keep.txt\n// FiLe: replacement.rs\n# FILE: shell.txt\n/* fIlE: block.txt */\nFILE: bare.rs\n//clipcode-esc: literal\n// clipcode-end\nlast");
	}
}
