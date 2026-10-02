//! 這個 module 擁有 cat-file --batch session、blob 可否複製的規則（spec 4.2「未複製」、porting-notes §4）以及刪除檔標記政策。

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

use crate::gitrun::{self, CancelToken, RunOptions};
use crate::gitsrc::{Git, GitError};

/// Content of a deleted file whose pre-deletion content no parent can supply.
pub const DELETED_FILE_MARKER: &str =
	"// This file has been deleted in this change";

/// A long-lived `git cat-file --batch`. Requests go one at a time (write,
/// flush, read the answer), so neither pipe can fill up and deadlock. Each
/// request has its own deadline.
pub struct CatFile {
	session: gitrun::Session,
}

/// One `cat-file --batch` answer read with a size cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatObject {
	Missing,
	/// The header fixed OID and size; the body was skipped unread.
	TooLarge {
		oid: String,
		size: u64,
	},
	Found {
		oid: String,
		kind: String,
		body: Vec<u8>,
	},
}

impl CatFile {
	pub(crate) fn spawn(
		cmd: std::process::Command,
		opts: RunOptions,
	) -> Result<CatFile, GitError> {
		Ok(CatFile {
			session: gitrun::Session::spawn(cmd, "cat-file --batch", opts)?,
		})
	}

	fn request(&mut self, object: &str) -> Result<(), GitError> {
		let Some(stdin) = self.session.begin() else {
			return Err(GitError::Malformed("cat-file stdin closed".into()));
		};
		let sent = stdin
			.write_all(format!("{object}\n").as_bytes())
			.and_then(|()| stdin.flush());
		sent.map_err(|e| self.session.error(e))
	}

	/// Reads an object (`<oid>` or `<rev>:<path>`). `None` = missing.
	pub fn read(&mut self, object: &str) -> Result<Option<Vec<u8>>, GitError> {
		if object.contains(['\n', '\r']) {
			// The protocol is line based; such a request would desync it.
			return Ok(None);
		}
		self.request(object)?;
		read_batch_response(&mut self.session)
			.map_err(|e| self.session.error(e))
	}

	/// Reads `object` only if its size, taken from the header before any
	/// body byte, is at most `max`. The returned OID is the object the body
	/// belongs to, so a ref moving meanwhile cannot mix two versions.
	pub fn read_object(
		&mut self,
		object: &str,
		max: u64,
	) -> Result<CatObject, GitError> {
		if object.contains(['\n', '\r']) {
			return Ok(CatObject::Missing);
		}
		self.request(object)?;
		let s = &mut self.session;
		let header = read_batch_header(s).map_err(|e| s.error(e))?;
		let Some((oid, kind, size)) = header else {
			return Ok(CatObject::Missing);
		};
		if size > max {
			skip_body(s, size).map_err(|e| {
				if e.kind() == io::ErrorKind::UnexpectedEof {
					GitError::Malformed("cat-file body truncated".into())
				} else {
					s.error(e)
				}
			})?;
			return Ok(CatObject::TooLarge { oid, size });
		}
		let body = read_batch_body(s, size).map_err(|e| s.error(e))?;
		Ok(CatObject::Found { oid, kind, body })
	}

	/// 讀取物件並分類為 [`BlobRead`]。
	///
	/// `cap` 為原始位元組上限（raw-byte cap），而非 JSON 序列化後的上限；需要檢查逸出後大小的呼叫端應在取得文字後自行計算。
	/// 大小判定先於型別判定；超限內容一律回傳 `TooLarge`。
	///
	/// 讀取錯誤（含 body 被截斷）經 `Session::error` 依子行程結束狀態回報 Failed / Io / Timeout。
	pub(crate) fn read_classified(
		&mut self,
		object: &str,
		cap: u64,
		require_blob: bool,
	) -> Result<BlobRead, GitError> {
		if object.contains(['\n', '\r']) {
			return Ok(BlobRead::Missing);
		}
		self.request(object)?;
		read_classified_response(&mut self.session, cap, require_blob)
			.map_err(|e| self.session.error(e))
	}

	/// Ends the batch and reports any cleanup failure. Dropping a
	/// `CatFile` kills the process instead.
	pub fn close(self) -> Result<(), GitError> {
		self.session.close()
	}
}

/// Reads one `cat-file --batch` header: `<oid> <type> <size>\n`, or
/// `<object> missing\n` / `ambiguous` (`None`).
pub fn read_batch_header<R: BufRead>(
	reader: &mut R,
) -> io::Result<Option<(String, String, u64)>> {
	let mut header = Vec::new();
	reader.read_until(b'\n', &mut header)?;
	if header.pop() != Some(b'\n') {
		return Err(io::ErrorKind::UnexpectedEof.into());
	}
	let header = String::from_utf8_lossy(&header);
	// "<oid> <type> <size>": size is the last field. `missing` and
	// `ambiguous` responses carry no body.
	let mut fields = header.rsplitn(3, ' ');
	let (Some(size), Some(kind), Some(oid)) =
		(fields.next(), fields.next(), fields.next())
	else {
		return Ok(None);
	};
	Ok(size
		.parse::<u64>()
		.ok()
		.map(|size| (oid.to_string(), kind.to_string(), size)))
}

fn read_batch_body<R: BufRead>(
	reader: &mut R,
	size: u64,
) -> io::Result<Vec<u8>> {
	let size = usize::try_from(size).map_err(io::Error::other)?;
	let mut body = vec![0; size];
	reader.read_exact(&mut body)?;
	let mut lf = [0u8; 1];
	reader.read_exact(&mut lf)?;
	Ok(body)
}

/// Reads one `git cat-file --batch` response: `<oid> <type> <size>\n`
/// followed by exactly `size` bytes and a LF, or `<object> missing\n`.
/// `None` means the object is missing (or ambiguous).
pub fn read_batch_response<R: BufRead>(
	reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
	match read_batch_header(reader)? {
		Some((_, _, size)) => read_batch_body(reader, size).map(Some),
		None => Ok(None),
	}
}

/// Consumes `size` bytes plus the trailing LF without storing it; a short
/// read is UnexpectedEof. Stay in protocol sync without holding the body.
fn skip_body<R: BufRead>(r: &mut R, size: u64) -> io::Result<()> {
	let skipped = io::copy(&mut r.take(size + 1), &mut io::sink())?;
	if skipped != size + 1 {
		return Err(io::ErrorKind::UnexpectedEof.into());
	}
	Ok(())
}

/// Consumes `size` bytes plus the trailing LF without storing it; NUL wins
/// over invalid UTF-8, matching a full-buffer check.
fn scan_discarded_body<R: BufRead>(
	reader: &mut R,
	size: u64,
) -> io::Result<Option<NotText>> {
	let mut left = size;
	let mut saw_nul = false;
	let mut invalid = false;
	let mut carry = Vec::new();
	let mut buf = [0u8; 8192];
	while left > 0 {
		let n = usize::try_from(left.min(buf.len() as u64))
			.map_err(io::Error::other)?;
		reader.read_exact(&mut buf[..n])?;
		left -= n as u64;
		if !saw_nul && buf[..n].contains(&0) {
			saw_nul = true;
		}
		if !saw_nul
			&& !invalid
			&& utf8_chunk_invalid(&mut carry, &buf[..n], left == 0)
		{
			invalid = true;
		}
	}
	let mut lf = [0u8; 1];
	reader.read_exact(&mut lf)?;
	if saw_nul {
		Ok(Some(NotText::Binary))
	} else if invalid {
		Ok(Some(NotText::NotUtf8))
	} else {
		Ok(None)
	}
}

/// An incomplete trailing sequence is kept in carry (at most 3 bytes) unless
/// this is the last chunk, where it is invalid; chunk is at most 8 KiB.
fn utf8_chunk_invalid(carry: &mut Vec<u8>, chunk: &[u8], eof: bool) -> bool {
	if carry.is_empty() {
		return match std::str::from_utf8(chunk) {
			Ok(_) => false,
			Err(e) => match e.error_len() {
				Some(_) => true,
				None => {
					carry.extend_from_slice(&chunk[e.valid_up_to()..]);
					eof
				}
			},
		};
	}
	let mut tmp = Vec::with_capacity(carry.len() + chunk.len());
	tmp.extend_from_slice(carry);
	tmp.extend_from_slice(chunk);
	carry.clear();
	match std::str::from_utf8(&tmp) {
		Ok(_) => false,
		Err(e) => match e.error_len() {
			Some(_) => true,
			None => {
				carry.extend_from_slice(&tmp[e.valid_up_to()..]);
				eof
			}
		},
	}
}

/// 內容為什麼不能當文字帶走（commit 模式：未複製原因；檔案模式：跳過）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotText {
	Binary,
	NotUtf8,
}

/// 唯一的可複製規則：任何位置有 NUL 就是 Binary；否則要是嚴格 UTF-8（保留 BOM），不然就是 NotUtf8。
pub(crate) fn classify(bytes: Vec<u8>) -> Result<String, NotText> {
	if bytes.contains(&0) {
		return Err(NotText::Binary);
	}
	String::from_utf8(bytes).map_err(|_| NotText::NotUtf8)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobRead {
	Missing,
	/// size <= cap 且 require_blob 為真時 kind != "blob"；body 以 skip_body 略過，不保留。
	NotABlob {
		kind: String,
	},
	Text(String),
	NotText(NotText),
	/// size > cap（不論 kind）；body 以 8 KiB 分塊掃描後丟棄，not_text 是 classify 對完整 body 會給的結論。
	///
	/// 超出容量的內容一律經由 [`scan_discarded_body`] 進行分塊掃描：此掃描僅耗費 CPU，記憶體用量嚴格維持有界（bounded），
	/// 保留此行為是為了讓刪除檔分類政策（`deleted_not_text`、`first_deleted_text`）能區分二進位與超限純文字。
	TooLarge {
		size: u64,
		not_text: Option<NotText>,
	},
}

/// 純函式，可用 Cursor 測試。
///
/// `cap` 為原始位元組上限（raw-byte cap），而非 JSON 序列化後的上限。
pub(crate) fn read_classified_response<R: BufRead>(
	r: &mut R,
	cap: u64,
	require_blob: bool,
) -> io::Result<BlobRead> {
	let header = match read_batch_header(r)? {
		None => return Ok(BlobRead::Missing),
		Some(h) => h,
	};
	let (_oid, kind, size) = header;
	if size > cap {
		let not_text = scan_discarded_body(r, size)?;
		return Ok(BlobRead::TooLarge { size, not_text });
	}
	if require_blob && kind != "blob" {
		skip_body(r, size)?;
		return Ok(BlobRead::NotABlob { kind });
	}
	let body = read_batch_body(r, size)?;
	match classify(body) {
		Ok(s) => Ok(BlobRead::Text(s)),
		Err(e) => Ok(BlobRead::NotText(e)),
	}
}

/// 每次只維持一個開啟的 `cat-file --batch` session，當 root 變更時重新開啟。
///
/// 不變量（runner-slot invariant）：持有 cat-file runner slot 的執行緒絕不能啟動第二個 Git 程序（以免同一執行緒同時佔用兩個全域 Git 程序預算 slot）。
pub(crate) struct BlobReader {
	open: Option<(PathBuf, CatFile)>,
	opts: RunOptions,
	require_blob: bool,
}

impl BlobReader {
	#[cfg(test)]
	pub(crate) fn is_open(&self) -> bool {
		self.open.is_some()
	}

	pub(crate) fn new(opts: &RunOptions) -> Self {
		Self {
			open: None,
			opts: opts.clone(),
			require_blob: false,
		}
	}

	pub(crate) fn blobs_only(opts: &RunOptions) -> Self {
		Self {
			open: None,
			opts: opts.clone(),
			require_blob: true,
		}
	}

	pub(crate) fn read(
		&mut self,
		git: &Git,
		spec: &str,
		cap: u64,
	) -> Result<BlobRead, GitError> {
		self.read_internal(git, spec, cap, self.require_blob)
	}

	pub(crate) fn read_lenient(
		&mut self,
		git: &Git,
		spec: &str,
		cap: u64,
	) -> Result<BlobRead, GitError> {
		self.read_internal(git, spec, cap, false)
	}

	fn read_internal(
		&mut self,
		git: &Git,
		spec: &str,
		cap: u64,
		require_blob: bool,
	) -> Result<BlobRead, GitError> {
		if self
			.opts
			.cancel
			.as_ref()
			.is_some_and(CancelToken::is_cancelled)
		{
			return Err(GitError::Cancelled {
				args: "cat-file --batch".into(),
			});
		}
		if self.open.as_ref().is_some_and(|(r, _)| r != git.root()) {
			self.close()?;
		}
		if self.open.is_none() {
			let cat = git.cat_file_with(self.opts.clone())?;
			self.open = Some((git.root().to_path_buf(), cat));
		}
		match self.open.as_mut() {
			Some((_, cat)) => cat.read_classified(spec, cap, require_blob),
			None => Err(GitError::Malformed("cat-file unavailable".into())),
		}
	}

	pub(crate) fn deleted_file_content<I, S>(
		&mut self,
		git: &Git,
		specs: I,
		cap: u64,
	) -> Result<DeletedContent, GitError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		first_deleted_text(specs, |s| self.read(git, s, cap))
	}

	pub(crate) fn deleted_not_text(
		&mut self,
		git: &Git,
		spec: &str,
	) -> Result<Option<NotText>, GitError> {
		let read = self.read(git, spec, 0)?;
		Ok(not_text_of(read))
	}

	pub(crate) fn close(&mut self) -> Result<(), GitError> {
		if let Some((_, cat)) = self.open.take() {
			cat.close()?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeletedContent {
	Text(String),
	Marker,
	TooLarge { spec: String, size: u64 },
}

fn first_deleted_text<I, S>(
	specs: I,
	mut read: impl FnMut(&str) -> Result<BlobRead, GitError>,
) -> Result<DeletedContent, GitError>
where
	I: IntoIterator<Item = S>,
	S: AsRef<str>,
{
	for spec in specs {
		let s = spec.as_ref();
		if s.is_empty() || s.bytes().all(|b| b == b'0') {
			continue;
		}
		match read(s)? {
			BlobRead::Text(t) => return Ok(DeletedContent::Text(t)),
			BlobRead::Missing | BlobRead::NotText(_) => {}
			BlobRead::NotABlob { kind } => return Err(not_a_file(s, &kind)),
			BlobRead::TooLarge {
				not_text: Some(_), ..
			} => {}
			BlobRead::TooLarge {
				size,
				not_text: None,
			} => {
				return Ok(DeletedContent::TooLarge {
					spec: s.to_string(),
					size,
				});
			}
		}
	}
	Ok(DeletedContent::Marker)
}

// kind 在此刻意被忽略：commits 的 admit_entry 以 is_special_mode 守門（commits.rs），
// gitlink 與 tree 不會走到這裡，所以呼叫端只會拿到 blob。
// NotText 與超限且非文字的 TooLarge{not_text: Some} 都對應到同一個原因。
pub(crate) fn not_text_of(read: BlobRead) -> Option<NotText> {
	match read {
		BlobRead::NotText(r)
		| BlobRead::TooLarge {
			not_text: Some(r), ..
		} => Some(r),
		_ => None,
	}
}

pub(crate) fn not_a_file(spec: &str, kind: &str) -> GitError {
	GitError::Malformed(format!("'{spec}' is a {kind}, not a file"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	fn batch(entries: &[(&str, &[u8])]) -> Vec<u8> {
		let mut out = Vec::new();
		for (oid, body) in entries {
			if *oid == "missing" {
				out.extend_from_slice(b"abc:gone.ts missing\n");
			} else {
				out.extend_from_slice(
					format!("{oid} blob {}\n", body.len()).as_bytes(),
				);
				out.extend_from_slice(body);
				out.push(b'\n');
			}
		}
		out
	}

	fn text(r: &mut Cursor<Vec<u8>>) -> Option<String> {
		read_batch_response(r)
			.unwrap()
			.and_then(|b| classify(b).ok())
	}

	#[test]
	fn cat_file_parses_blobs_in_request_order() {
		let mut r = Cursor::new(batch(&[
			("1111", b"const a = 1;\n"),
			("2222", b"export const b = 2;"),
		]));
		assert_eq!(text(&mut r).as_deref(), Some("const a = 1;\n"));
		assert_eq!(text(&mut r).as_deref(), Some("export const b = 2;"));
	}

	#[test]
	fn cat_file_reads_by_byte_count_not_lines() {
		let tricky = "line1\n0000 blob 5\nline2";
		let mut r = Cursor::new(batch(&[
			("aaaa", tricky.as_bytes()),
			("bbbb", b"next"),
		]));
		assert_eq!(text(&mut r).as_deref(), Some(tricky));
		assert_eq!(text(&mut r).as_deref(), Some("next"));
	}

	#[test]
	fn cat_file_missing_is_distinct_and_keeps_alignment() {
		let mut r =
			Cursor::new(batch(&[("missing", b""), ("3333", b"still here")]));
		assert_eq!(read_batch_response(&mut r).unwrap(), None);
		assert_eq!(text(&mut r).as_deref(), Some("still here"));
	}

	#[test]
	fn cat_file_binary_and_non_utf8_are_skipped() {
		let mut r = Cursor::new(batch(&[
			("4444", &[0x50, 0x4e, 0x47, 0x00, 0x44]),
			("aaa", &[0xa4, 0xe9, 0xa5, 0xbb]), // Big5
			("bbb", b"ok"),
		]));
		assert_eq!(text(&mut r), None);
		assert_eq!(text(&mut r), None);
		assert_eq!(text(&mut r).as_deref(), Some("ok"));
	}

	#[test]
	fn cat_file_empty_blob_is_empty_not_missing() {
		let mut r = Cursor::new(batch(&[("5555", b"")]));
		assert_eq!(read_batch_response(&mut r).unwrap(), Some(Vec::new()));
	}

	#[test]
	fn cat_file_truncated_output_is_an_error_after_good_entries() {
		let mut r =
			Cursor::new(b"6666 blob 4\nabcd\n7777 blob 100\nshort".to_vec());
		assert_eq!(text(&mut r).as_deref(), Some("abcd"));
		assert!(read_batch_response(&mut r).is_err());
	}

	fn classify_discarded(body: &[u8]) -> Option<NotText> {
		let mut raw = body.to_vec();
		raw.push(b'\n');
		scan_discarded_body(&mut Cursor::new(raw), body.len() as u64).unwrap()
	}

	#[test]
	fn discarded_body_scan_matches_full_buffer_classification() {
		let mut split = vec![b'a'; 8191];
		split.extend_from_slice("你".as_bytes());
		assert_eq!(classify_discarded(&split), None);
		let mut broken = vec![b'a'; 8191];
		broken.push(0xE4);
		broken.push(b' ');
		assert_eq!(classify_discarded(&broken), Some(NotText::NotUtf8));
		let mut binary = vec![b'a'; 9000];
		binary.push(0);
		assert_eq!(classify_discarded(&binary), Some(NotText::Binary));
	}

	#[test]
	fn classify_keeps_bom_and_empty_is_text() {
		assert_eq!(
			classify(b"\xEF\xBB\xBFhello".to_vec()),
			Ok("\u{FEFF}hello".to_string())
		);
		assert_eq!(classify(Vec::new()), Ok(String::new()));
	}

	#[test]
	fn classify_nul_anywhere_is_binary_even_past_8000_bytes() {
		let mut b = vec![b'a'; 9000];
		b.push(0);
		assert_eq!(classify(b), Err(NotText::Binary));
	}

	#[test]
	fn classify_nul_wins_over_invalid_utf8() {
		assert_eq!(classify(vec![0xFF, 0x00]), Err(NotText::Binary));
		assert_eq!(classify(vec![0xa4, 0xe9]), Err(NotText::NotUtf8));
	}

	#[test]
	fn scan_agrees_with_classify_on_every_sample() {
		let mut boundary_char = vec![b'a'; 8191];
		boundary_char.extend_from_slice("你".as_bytes());

		let mut boundary_invalid = vec![b'a'; 8191];
		boundary_invalid.push(0xE4);
		boundary_invalid.push(b' ');

		let mut late_nul = vec![b'a'; 9000];
		late_nul.push(0);

		let empty = Vec::new();
		let bom = b"\xEF\xBB\xBFtext".to_vec();

		let samples =
			vec![boundary_char, boundary_invalid, late_nul, empty, bom];
		for s in samples {
			let want = classify(s.clone()).err();
			let got = classify_discarded(&s);
			assert_eq!(got, want);
		}
	}

	#[test]
	fn read_classified_response_cases() {
		// 1. missing 之後仍對齊
		let mut r = Cursor::new(batch(&[("missing", b""), ("1111", b"ok\n")]));
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Missing
		);
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Text("ok\n".into())
		);

		// 2. Text
		let mut r = Cursor::new(batch(&[("1111", b"hello")]));
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Text("hello".into())
		);

		// 3. NotText(Binary)
		let mut r = Cursor::new(batch(&[("1111", b"a\0b")]));
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::NotText(NotText::Binary)
		);

		// 4. cap 以下的非 blob：require_blob=true 回 NotABlob 且之後仍對齊；require_blob=false 解碼為 Text（忽略 kind）
		let mut r =
			Cursor::new(b"abc tree 5\n12345\nnext blob 3\nxyz\n".to_vec());
		assert_eq!(
			read_classified_response(&mut r, 1024, true).unwrap(),
			BlobRead::NotABlob {
				kind: "tree".into()
			}
		);
		assert_eq!(
			read_classified_response(&mut r, 1024, true).unwrap(),
			BlobRead::Text("xyz".into())
		);

		let mut r =
			Cursor::new(b"abc tree 5\n12345\nnext blob 3\nxyz\n".to_vec());
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Text("12345".into())
		);
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Text("xyz".into())
		);

		let commit_body = b"tree abc\nauthor Me <m@e>\n\ncommit msg\n";
		let raw = format!("c0ffee commit {}\n", commit_body.len()).into_bytes();
		let mut payload = raw;
		payload.extend_from_slice(commit_body);
		payload.push(b'\n');
		let mut r = Cursor::new(payload);
		assert_eq!(
			read_classified_response(&mut r, 1024, false).unwrap(),
			BlobRead::Text(String::from_utf8(commit_body.to_vec()).unwrap())
		);

		// 5. 文字、二進位、非 UTF-8 各一筆大於 cap 的，回 TooLarge 並帶正確的 not_text 且之後仍對齊
		let mut r = Cursor::new(batch(&[
			("1", b"long text content"),
			("2", b"bin\0content here"),
			("3", &[0xa4, 0xe9, 0xa5, 0xbb]),
			("4", b"aligned"),
		]));
		assert_eq!(
			read_classified_response(&mut r, 4, false).unwrap(),
			BlobRead::TooLarge {
				size: 17,
				not_text: None
			}
		);
		assert_eq!(
			read_classified_response(&mut r, 4, false).unwrap(),
			BlobRead::TooLarge {
				size: 16,
				not_text: Some(NotText::Binary)
			}
		);
		assert_eq!(
			read_classified_response(&mut r, 2, false).unwrap(),
			BlobRead::TooLarge {
				size: 4,
				not_text: Some(NotText::NotUtf8)
			}
		);
		assert_eq!(
			read_classified_response(&mut r, 10, false).unwrap(),
			BlobRead::Text("aligned".into())
		);

		// 6. 大於 cap 的 tree 回 TooLarge（先比 size，kind 忽略）
		let mut r =
			Cursor::new(b"abc tree 20\n12345678901234567890\n".to_vec());
		assert_eq!(
			read_classified_response(&mut r, 5, true).unwrap(),
			BlobRead::TooLarge {
				size: 20,
				not_text: None
			}
		);

		// 7. cap 0 的空 blob 回 Text("")
		let mut r = Cursor::new(batch(&[("empty", b"")]));
		assert_eq!(
			read_classified_response(&mut r, 0, false).unwrap(),
			BlobRead::Text("".into())
		);

		// 8. body 被截斷回 Err
		let mut r = Cursor::new(b"abc blob 20\nshort".to_vec());
		assert!(read_classified_response(&mut r, 1024, false).is_err());
	}

	#[test]
	fn cat_file_short_skipped_body_is_err() {
		let mut r_overcap = Cursor::new(b"abc blob 20\nshort".to_vec());
		assert!(read_classified_response(&mut r_overcap, 5, false).is_err());

		let mut r_nonblob = Cursor::new(b"abc tree 20\nshort".to_vec());
		assert!(read_classified_response(&mut r_nonblob, 1024, true).is_err());
	}

	#[test]
	fn deleted_policy_binary_then_text_picks_text() {
		let specs = ["bin", "txt"];
		let res = first_deleted_text(specs, |s| match s {
			"bin" => Ok(BlobRead::NotText(NotText::Binary)),
			"txt" => Ok(BlobRead::Text("recovered".into())),
			_ => unreachable!(),
		})
		.unwrap();
		assert_eq!(res, DeletedContent::Text("recovered".into()));
	}

	#[test]
	fn deleted_policy_all_uncopyable_is_marker() {
		let specs = ["missing", "bin", "huge_bin"];
		let res = first_deleted_text(specs, |s| match s {
			"missing" => Ok(BlobRead::Missing),
			"bin" => Ok(BlobRead::NotText(NotText::Binary)),
			"huge_bin" => Ok(BlobRead::TooLarge {
				size: 100_000,
				not_text: Some(NotText::Binary),
			}),
			_ => unreachable!(),
		})
		.unwrap();
		assert_eq!(res, DeletedContent::Marker);
	}

	#[test]
	fn deleted_policy_not_a_blob_returns_malformed() {
		let specs = ["tree"];
		let err = first_deleted_text(specs, |_| {
			Ok(BlobRead::NotABlob {
				kind: "tree".into(),
			})
		})
		.unwrap_err();
		match err {
			GitError::Malformed(msg) => {
				assert_eq!(msg, "'tree' is a tree, not a file");
			}
			other => panic!("expected GitError::Malformed, got {other:?}"),
		}
	}

	#[test]
	fn deleted_policy_skips_zero_and_empty_specs_without_reading() {
		let mut visited = Vec::new();
		let specs = ["", "0000000000000000000000000000000000000000", "real"];
		let res = first_deleted_text(specs, |s| {
			visited.push(s.to_string());
			Ok(BlobRead::Text("content".into()))
		})
		.unwrap();
		assert_eq!(visited, vec!["real"]);
		assert_eq!(res, DeletedContent::Text("content".into()));
	}

	#[test]
	fn deleted_policy_oversize_text_stops_and_names_spec() {
		let mut visited = Vec::new();
		let specs = ["huge_txt", "next"];
		let res = first_deleted_text(specs, |s| {
			visited.push(s.to_string());
			match s {
				"huge_txt" => Ok(BlobRead::TooLarge {
					size: 50_000,
					not_text: None,
				}),
				_ => Ok(BlobRead::Text("next".into())),
			}
		})
		.unwrap();
		assert_eq!(visited, vec!["huge_txt"]);
		assert_eq!(
			res,
			DeletedContent::TooLarge {
				spec: "huge_txt".into(),
				size: 50_000,
			}
		);
	}

	#[test]
	fn not_text_of_maps_every_variant() {
		assert_eq!(
			not_text_of(BlobRead::NotText(NotText::Binary)),
			Some(NotText::Binary)
		);
		assert_eq!(
			not_text_of(BlobRead::NotText(NotText::NotUtf8)),
			Some(NotText::NotUtf8)
		);
		assert_eq!(
			not_text_of(BlobRead::TooLarge {
				size: 10,
				not_text: Some(NotText::Binary),
			}),
			Some(NotText::Binary)
		);
		assert_eq!(
			not_text_of(BlobRead::TooLarge {
				size: 10,
				not_text: Some(NotText::NotUtf8),
			}),
			Some(NotText::NotUtf8)
		);
		assert_eq!(not_text_of(BlobRead::Text("text".into())), None);
		assert_eq!(not_text_of(BlobRead::Missing), None);
		assert_eq!(
			not_text_of(BlobRead::NotABlob {
				kind: "tree".into()
			}),
			None
		);
		assert_eq!(
			not_text_of(BlobRead::TooLarge {
				size: 10,
				not_text: None,
			}),
			None
		);
	}
}
