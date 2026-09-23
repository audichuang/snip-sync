//! User settings shared by the CLI and desktop app.
//!
//! Port of `settings.ts`. Field names serialize exactly as the TS
//! `ClipCodeSettings` so the same JSON settings file works for both.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FilterType {
	Path,
	Pattern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FilterAction {
	Include,
	Exclude,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterRule {
	#[serde(rename = "type")]
	pub kind: FilterType,
	pub action: FilterAction,
	pub value: String,
	pub enabled: bool,
}

/// Mirror of `ClipCodeSettings`. The two limits are `f64` because TS keeps
/// them as plain numbers (a fractional limit is accepted there too).
///
/// Deserializing always goes through [`normalize`], so there is no
/// unvalidated way to build one from JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
	pub header_format: String,
	pub pre_text: String,
	pub post_text: String,
	pub add_extra_line_between_files: bool,
	pub set_max_file_count: bool,
	pub file_count_limit: f64,
	#[serde(rename = "maxFileSizeKB")]
	pub max_file_size_kb: f64,
	pub show_copy_notification: bool,
	pub use_filters: bool,
	pub use_include_filters: bool,
	pub use_exclude_filters: bool,
	pub filter_rules: Vec<FilterRule>,
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			header_format: "// file: $FILE_PATH".to_string(),
			pre_text: String::new(),
			post_text: String::new(),
			add_extra_line_between_files: true,
			set_max_file_count: true,
			file_count_limit: 30.0,
			max_file_size_kb: 500.0,
			show_copy_notification: true,
			use_filters: false,
			use_include_filters: true,
			use_exclude_filters: true,
			filter_rules: Vec::new(),
		}
	}
}

impl<'de> Deserialize<'de> for Settings {
	fn deserialize<D: serde::Deserializer<'de>>(
		d: D,
	) -> Result<Self, D::Error> {
		Ok(normalize(&Value::deserialize(d)?))
	}
}

/// Port of `normalizeSettings`: defaults overlaid with whatever `input`
/// provides, non-positive limits reset to their defaults, and malformed
/// filter rules dropped.
///
/// TS spreads `input` blindly, so a wrongly typed value would pass through;
/// that cannot be represented here, so such a field keeps its default.
pub fn normalize(input: &Value) -> Settings {
	let mut s = Settings::default();
	let string = |key: &str, slot: &mut String| {
		if let Some(v) = input.get(key).and_then(Value::as_str) {
			*slot = v.to_string();
		}
	};
	string("headerFormat", &mut s.header_format);
	string("preText", &mut s.pre_text);
	string("postText", &mut s.post_text);

	let boolean = |key: &str, slot: &mut bool| {
		if let Some(v) = input.get(key).and_then(Value::as_bool) {
			*slot = v;
		}
	};
	boolean(
		"addExtraLineBetweenFiles",
		&mut s.add_extra_line_between_files,
	);
	boolean("setMaxFileCount", &mut s.set_max_file_count);
	boolean("showCopyNotification", &mut s.show_copy_notification);
	boolean("useFilters", &mut s.use_filters);
	boolean("useIncludeFilters", &mut s.use_include_filters);
	boolean("useExcludeFilters", &mut s.use_exclude_filters);

	let positive = |key: &str, slot: &mut f64| {
		if let Some(v) = input.get(key).and_then(Value::as_f64) {
			if v.is_finite() && v > 0.0 {
				*slot = v;
			}
		}
	};
	positive("fileCountLimit", &mut s.file_count_limit);
	positive("maxFileSizeKB", &mut s.max_file_size_kb);

	if let Some(rules) = input.get("filterRules").and_then(Value::as_array) {
		// Same shape check as isFilterRule: known type/action, string value,
		// boolean enabled.
		s.filter_rules = rules
			.iter()
			.filter_map(|r| serde_json::from_value(r.clone()).ok())
			.collect();
	}
	s
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn defaults_match_intellij_clipcode_settings() {
		let d = Settings::default();
		assert_eq!(d.header_format, "// file: $FILE_PATH");
		assert_eq!(d.file_count_limit, 30.0);
		assert_eq!(d.max_file_size_kb, 500.0);
		assert!(d.add_extra_line_between_files);
	}

	#[test]
	fn filter_rule_type_is_preserved_explicitly() {
		let s = normalize(&json!({
			"filterRules": [
				{ "type": "PATH", "action": "EXCLUDE", "value": ".github", "enabled": true }
			]
		}));
		assert_eq!(
			s.filter_rules[0],
			FilterRule {
				kind: FilterType::Path,
				action: FilterAction::Exclude,
				value: ".github".to_string(),
				enabled: true,
			}
		);
	}

	#[test]
	fn normalize_drops_bad_rules_and_non_positive_limits() {
		let s = normalize(&json!({
			"fileCountLimit": 0,
			"maxFileSizeKB": -5,
			"headerFormat": "## $FILE_PATH",
			"filterRules": [
				{ "type": "GLOB", "action": "EXCLUDE", "value": "x", "enabled": true },
				{ "type": "PATH", "action": "EXCLUDE", "value": 1, "enabled": true },
				{ "type": "PATH", "action": "EXCLUDE", "value": "x" },
				"PATH",
				{ "type": "PATTERN", "action": "INCLUDE", "value": "*.rs", "enabled": false }
			]
		}));
		assert_eq!(s.file_count_limit, 30.0);
		assert_eq!(s.max_file_size_kb, 500.0);
		assert_eq!(s.header_format, "## $FILE_PATH");
		assert_eq!(s.filter_rules.len(), 1);
		assert_eq!(s.filter_rules[0].kind, FilterType::Pattern);
		assert_eq!(normalize(&json!({})), Settings::default());
	}

	#[test]
	fn serializes_with_ts_field_names() {
		let v = serde_json::to_value(Settings::default()).unwrap();
		for key in [
			"headerFormat",
			"preText",
			"postText",
			"addExtraLineBetweenFiles",
			"setMaxFileCount",
			"fileCountLimit",
			"maxFileSizeKB",
			"showCopyNotification",
			"useFilters",
			"useIncludeFilters",
			"useExcludeFilters",
			"filterRules",
		] {
			assert!(v.get(key).is_some(), "missing {key}");
		}
		assert_eq!(normalize(&v), Settings::default());
		let parsed: Settings =
			serde_json::from_str(r#"{"fileCountLimit":0,"filterRules":[1]}"#)
				.unwrap();
		assert_eq!(parsed, Settings::default());
	}
}
