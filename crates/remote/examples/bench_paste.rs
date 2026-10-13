//! Measurement driver for scripts/bench_remote_paste.py. Both protocol modes
//! execute the same public PasteTarget path; barriers keep this PID alive for
//! per-process CPU/RSS collection instead of cumulative child resource usage.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
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
	let config: Config = serde_json::from_slice(&fs::read(
		std::env::args_os()
			.nth(1)
			.ok_or("expected config JSON path")?,
	)?)?;
	let text = fs::read_to_string(&config.payload)?;
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
	let output = match &result {
		Ok((prepared, applied)) => {
			json!({"pid":std::process::id(),"elapsed_seconds":started.elapsed().as_secs_f64(),"prepared":prepared,"result":applied})
		}
		Err(error) => {
			json!({"pid":std::process::id(),"elapsed_seconds":started.elapsed().as_secs_f64(),"error":error.to_string()})
		}
	};
	publish(&config.control.join("done.json"), &output)?;
	if !config.oracle {
		barrier(&config.control.join("release"))?;
	}
	result.map(|_| ())
}
