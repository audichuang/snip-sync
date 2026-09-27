//! Lightweight syntax highlighting for common programming languages.
//!
//! Supports Rust, TypeScript/JavaScript, Python, JSON, and unified Git diffs.
//! No heavy dependencies (no LSP, Monaco, tree-sitter); minimal tokenization
//! optimized for fast GPUI row rendering.

use gpui::{rgb, Rgba};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
	Rust,
	TypeScript,
	Python,
	Json,
	Diff,
	Plain,
}

impl Language {
	pub fn from_path_or_ext(path: &str, is_diff: bool) -> Self {
		if is_diff {
			return Self::Diff;
		}
		let lower = path.to_ascii_lowercase();
		if lower.ends_with(".rs") {
			Self::Rust
		} else if lower.ends_with(".ts")
			|| lower.ends_with(".tsx")
			|| lower.ends_with(".js")
			|| lower.ends_with(".jsx")
			|| lower.ends_with(".mjs")
			|| lower.ends_with(".cjs")
		{
			Self::TypeScript
		} else if lower.ends_with(".py") {
			Self::Python
		} else if lower.ends_with(".json") {
			Self::Json
		} else if lower.ends_with(".diff") || lower.ends_with(".patch") {
			Self::Diff
		} else {
			Self::Plain
		}
	}
}

pub struct SyntaxTheme {
	pub text: Rgba,
	pub keyword: Rgba,
	pub string: Rgba,
	pub number: Rgba,
	pub comment: Rgba,
	pub type_name: Rgba,
	pub punctuation: Rgba,
	pub diff_add: Rgba,
	pub diff_remove: Rgba,
	pub diff_hunk: Rgba,
}

impl Default for SyntaxTheme {
	/// The active palette's editor scheme.
	fn default() -> Self {
		let p = crate::theme::pal();
		Self {
			text: rgb(p.code_text),
			keyword: rgb(p.syntax_keyword),
			string: rgb(p.syntax_string),
			number: rgb(p.syntax_number),
			comment: rgb(p.syntax_comment),
			type_name: rgb(p.syntax_type),
			punctuation: rgb(p.code_text),
			diff_add: rgb(p.git_added),
			diff_remove: rgb(p.error),
			diff_hunk: rgb(p.diff_hunk_text),
		}
	}
}

pub fn highlight_line(
	line: &str,
	lang: Language,
	theme: &SyntaxTheme,
) -> Vec<(String, Rgba)> {
	if line.is_empty() {
		return vec![(" ".to_string(), theme.text)];
	}

	match lang {
		Language::Diff => highlight_diff(line, theme),
		Language::Rust => highlight_general(line, lang, theme),
		Language::TypeScript => highlight_general(line, lang, theme),
		Language::Python => highlight_general(line, lang, theme),
		Language::Json => highlight_json(line, theme),
		Language::Plain => vec![(line.to_string(), theme.text)],
	}
}

fn highlight_diff(line: &str, theme: &SyntaxTheme) -> Vec<(String, Rgba)> {
	if (line.starts_with('+') && !line.starts_with("+++"))
		|| line.starts_with("new file mode")
	{
		vec![(line.to_string(), theme.diff_add)]
	} else if (line.starts_with('-') && !line.starts_with("---"))
		|| line.starts_with("deleted file mode")
	{
		vec![(line.to_string(), theme.diff_remove)]
	} else if line.starts_with('@') {
		vec![(line.to_string(), theme.diff_hunk)]
	} else if line.starts_with("diff --git")
		|| line.starts_with("index ")
		|| line.starts_with("--- ")
		|| line.starts_with("+++ ")
	{
		vec![(line.to_string(), theme.type_name)]
	} else {
		vec![(line.to_string(), theme.text)]
	}
}

fn is_keyword(word: &str, lang: Language) -> bool {
	match lang {
		Language::Rust => matches!(
			word,
			"as" | "break"
				| "const" | "continue"
				| "crate" | "else"
				| "enum" | "extern"
				| "false" | "fn"
				| "for" | "if"
				| "impl" | "in"
				| "let" | "loop"
				| "match" | "mod"
				| "move" | "mut"
				| "pub" | "ref"
				| "return" | "self"
				| "Self" | "static"
				| "struct" | "super"
				| "trait" | "true"
				| "type" | "unsafe"
				| "use" | "where"
				| "while" | "async"
				| "await" | "dyn"
		),
		Language::TypeScript => {
			matches!(
				word,
				"break"
					| "case" | "catch"
					| "class" | "const"
					| "continue" | "debugger"
					| "default" | "delete"
					| "do" | "else" | "export"
					| "extends" | "finally"
					| "for" | "function"
					| "if" | "import"
					| "in" | "instanceof"
					| "new" | "return"
					| "super" | "switch"
					| "this" | "throw"
					| "try" | "typeof"
					| "var" | "void"
					| "while" | "with"
					| "yield" | "let"
					| "static" | "interface"
					| "type" | "as" | "from"
					| "async" | "await"
					| "true" | "false"
					| "null" | "undefined"
			)
		}
		Language::Python => {
			matches!(
				word,
				"False"
					| "None" | "True"
					| "and" | "as" | "assert"
					| "async" | "await"
					| "break" | "class"
					| "continue" | "def"
					| "del" | "elif"
					| "else" | "except"
					| "finally" | "for"
					| "from" | "global"
					| "if" | "import"
					| "in" | "is" | "lambda"
					| "nonlocal" | "not"
					| "or" | "pass" | "raise"
					| "return" | "try"
					| "while" | "with"
					| "yield" | "self"
			)
		}
		_ => false,
	}
}

fn highlight_general(
	line: &str,
	lang: Language,
	theme: &SyntaxTheme,
) -> Vec<(String, Rgba)> {
	let mut tokens = Vec::new();
	let chars: Vec<char> = line.chars().collect();
	let len = chars.len();
	let mut i = 0;

	while i < len {
		// Check for line comments
		if (lang != Language::Python
			&& i + 1 < len
			&& chars[i] == '/'
			&& chars[i + 1] == '/')
			|| (lang == Language::Python && chars[i] == '#')
		{
			let comment: String = chars[i..].iter().collect();
			tokens.push((comment, theme.comment));
			break;
		}

		// String literals (double or single quotes)
		if chars[i] == '"' || chars[i] == '\'' || chars[i] == '`' {
			let quote = chars[i];
			let mut s = String::new();
			s.push(quote);
			i += 1;
			while i < len {
				s.push(chars[i]);
				if chars[i] == '\\' && i + 1 < len {
					i += 1;
					s.push(chars[i]);
				} else if chars[i] == quote {
					i += 1;
					break;
				}
				i += 1;
			}
			tokens.push((s, theme.string));
			continue;
		}

		// Numbers
		if chars[i].is_ascii_digit() {
			let mut num = String::new();
			while i < len
				&& (chars[i].is_ascii_alphanumeric()
					|| chars[i] == '.'
					|| chars[i] == '_')
			{
				num.push(chars[i]);
				i += 1;
			}
			tokens.push((num, theme.number));
			continue;
		}

		// Identifiers / Keywords
		if chars[i].is_alphabetic() || chars[i] == '_' {
			let mut word = String::new();
			while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
				word.push(chars[i]);
				i += 1;
			}
			let color = if is_keyword(&word, lang) {
				theme.keyword
			} else if word.chars().next().is_some_and(|c| c.is_uppercase()) {
				theme.type_name
			} else {
				theme.text
			};
			tokens.push((word, color));
			continue;
		}

		// Whitespace
		if chars[i].is_whitespace() {
			let mut ws = String::new();
			while i < len && chars[i].is_whitespace() {
				ws.push(chars[i]);
				i += 1;
			}
			tokens.push((ws, theme.text));
			continue;
		}

		// Punctuation & Operators
		let mut punct = String::new();
		punct.push(chars[i]);
		i += 1;
		tokens.push((punct, theme.punctuation));
	}

	tokens
}

fn highlight_json(line: &str, theme: &SyntaxTheme) -> Vec<(String, Rgba)> {
	let mut tokens = Vec::new();
	let chars: Vec<char> = line.chars().collect();
	let len = chars.len();
	let mut i = 0;

	while i < len {
		if chars[i] == '"' {
			let mut s = String::new();
			s.push('"');
			i += 1;
			while i < len {
				s.push(chars[i]);
				if chars[i] == '\\' && i + 1 < len {
					i += 1;
					s.push(chars[i]);
				} else if chars[i] == '"' {
					i += 1;
					break;
				}
				i += 1;
			}
			// Check if this string is a JSON key (followed by whitespace then ':')
			let mut peek = i;
			while peek < len && chars[peek].is_whitespace() {
				peek += 1;
			}
			let color = if peek < len && chars[peek] == ':' {
				theme.type_name // JSON keys highlighted distinctly
			} else {
				theme.string
			};
			tokens.push((s, color));
			continue;
		}

		if chars[i].is_ascii_digit() || chars[i] == '-' {
			let mut num = String::new();
			num.push(chars[i]);
			i += 1;
			while i < len
				&& (chars[i].is_ascii_digit()
					|| chars[i] == '.'
					|| chars[i] == 'e'
					|| chars[i] == 'E'
					|| chars[i] == '+'
					|| chars[i] == '-')
			{
				num.push(chars[i]);
				i += 1;
			}
			tokens.push((num, theme.number));
			continue;
		}

		if chars[i].is_alphabetic() {
			let mut word = String::new();
			while i < len && chars[i].is_alphabetic() {
				word.push(chars[i]);
				i += 1;
			}
			let color = if word == "true" || word == "false" || word == "null" {
				theme.keyword
			} else {
				theme.text
			};
			tokens.push((word, color));
			continue;
		}

		if chars[i].is_whitespace() {
			let mut ws = String::new();
			while i < len && chars[i].is_whitespace() {
				ws.push(chars[i]);
				i += 1;
			}
			tokens.push((ws, theme.text));
			continue;
		}

		let mut punct = String::new();
		punct.push(chars[i]);
		i += 1;
		tokens.push((punct, theme.punctuation));
	}

	tokens
}
