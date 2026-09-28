//! Shared helpers for the real-app drivers in `tests/`.

use std::path::PathBuf;
use std::time::Duration;

/// The app under test: `SNIP_NATIVE_BIN`, set by `just native-smoke`,
/// `just native-lifecycle` and the acceptance shards. There is no fallback
/// build, so the executable a run reports is the one it drove.
pub fn native_bin() -> PathBuf {
	let bin = std::env::var_os("SNIP_NATIVE_BIN")
		.map(PathBuf::from)
		.expect(
		"SNIP_NATIVE_BIN must name the snip-desktop-native executable to drive",
	);
	assert!(bin.is_file(), "SNIP_NATIVE_BIN {bin:?} is not a file");
	bin
}

/// Every wait deadline goes through here. `SNIP_E2E_TIMEOUT_SCALE` (e.g. 2 in
/// CI) stretches them for a loaded machine: lavapipe renders on the CPU, so a
/// busy host makes a healthy app miss fixed deadlines.
pub fn scaled(d: Duration) -> Duration {
	static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
	d.mul_f64(*SCALE.get_or_init(|| {
		std::env::var("SNIP_E2E_TIMEOUT_SCALE")
			.ok()
			.and_then(|v| v.parse::<f64>().ok())
			.filter(|v| v.is_finite() && *v >= 1.0)
			.unwrap_or(1.0)
	}))
}
