//! Text statistics: chars, lines, words and token estimates.
//!
//! Port of `payloadStats` / `estimateTokens` from ClipCodeVSCode `src/copy.ts`.
//! One linear pass with O(1) extra memory, computed over the whole payload.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayloadStats {
	/// UTF-16 code units, like JS/Kotlin `String.length`.
	pub chars: usize,
	/// Newline count + 1, or 0 for empty text. A lone `\r` is not a break.
	pub lines: usize,
	/// Maximal runs of non-separators, split on ASCII whitespace only.
	pub words: usize,
	/// `words` plus occurrences of `;,(){}[]`.
	pub tokens: usize,
}

pub fn payload_stats(text: &str) -> PayloadStats {
	let mut words = 0;
	let mut punctuation = 0;
	let mut newlines = 0;
	let mut in_word = false;
	// Byte scan is exact: every byte of a multi-byte UTF-8 char is >= 0x80, so it
	// never matches a separator or punctuation and only ever extends a word.
	for &b in text.as_bytes() {
		if b == b'\n' {
			newlines += 1;
		}
		// Space plus 0x09-0x0D (\t \n \x0B \f \r); u8::is_ascii_whitespace lacks \x0B.
		if b == b' ' || (0x09..=0x0D).contains(&b) {
			in_word = false;
			continue;
		}
		if !in_word {
			in_word = true;
			words += 1;
		}
		if matches!(b, b';' | b',' | b'(' | b')' | b'{' | b'}' | b'[' | b']') {
			punctuation += 1;
		}
	}
	PayloadStats {
		chars: text.encode_utf16().count(),
		lines: if text.is_empty() { 0 } else { newlines + 1 },
		words,
		tokens: words + punctuation,
	}
}

/// The `tokens` field alone; the name the contract fixture uses.
pub fn estimate_tokens(text: &str) -> usize {
	payload_stats(text).tokens
}

// Ported from ClipCodeVSCode test/estimateTokens.test.ts.
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_string_has_no_tokens() {
		assert_eq!(estimate_tokens(""), 0);
		assert_eq!(estimate_tokens("   \n  "), 0);
	}

	#[test]
	fn counts_whitespace_separated_words() {
		assert_eq!(estimate_tokens("hello world"), 2);
	}

	#[test]
	fn splits_on_ascii_whitespace_only() {
		assert_eq!(estimate_tokens("\u{4E2D}\u{3000}\u{6587}"), 1); // ideographic space
		assert_eq!(estimate_tokens("a\u{00A0}b"), 1); // NBSP
		assert_eq!(estimate_tokens("a b"), 2);
		assert_eq!(estimate_tokens("a\x0Bb"), 2); // vertical tab is a separator
	}

	#[test]
	fn adds_structural_punctuation_to_word_count() {
		assert_eq!(estimate_tokens("foo(bar);"), 4);
		assert_eq!(estimate_tokens("a, b, c"), 5);
	}

	#[test]
	fn counts_chars_as_utf16_code_units() {
		assert_eq!(payload_stats("abc").chars, 3);
		assert_eq!(payload_stats("\u{4E2D}\u{6587}").chars, 2);
		assert_eq!(payload_stats("\u{1F44D}").chars, 2); // astral char = 2 units
	}

	#[test]
	fn counts_lines_as_newlines_plus_one() {
		assert_eq!(payload_stats("").lines, 0);
		assert_eq!(payload_stats("one line").lines, 1);
		assert_eq!(payload_stats("a\nb").lines, 2);
		assert_eq!(payload_stats("a\nb\n").lines, 3);
		assert_eq!(payload_stats("a\r\nb\rc").lines, 2);
	}

	#[test]
	fn words_is_tokens_without_punctuation_bonus() {
		assert_eq!(payload_stats("foo(bar);").words, 1);
		assert_eq!(payload_stats("foo(bar);").tokens, 4);
		assert_eq!(payload_stats("a, b, c").words, 3);
	}
}
