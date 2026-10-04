//! Internationalization (i18n) dictionary for snip-desktop-native.
//!
//! Provides clean zh-TW (traditional Chinese, default) and en (English) translations
//! to avoid messy inline mixed bilingual strings.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
	#[default]
	ZhTw,
	En,
}

pub trait IntoMsgArgs {
	fn into_args(self) -> Vec<String>;
}

impl<const N: usize> IntoMsgArgs for [String; N] {
	fn into_args(self) -> Vec<String> {
		self.into_iter().collect()
	}
}

impl IntoMsgArgs for Vec<String> {
	fn into_args(self) -> Vec<String> {
		self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
	pub key: &'static str,
	pub args: Vec<String>,
	pub key_args: Vec<usize>,
}

impl Msg {
	pub fn new(key: &'static str, args: impl IntoMsgArgs) -> Self {
		Self {
			key,
			args: args.into_args(),
			key_args: Vec::new(),
		}
	}

	pub fn with_key_arg(
		key: &'static str,
		args: impl IntoMsgArgs,
		index: usize,
	) -> Self {
		Self {
			key,
			args: args.into_args(),
			key_args: vec![index],
		}
	}

	pub fn with_key_args(
		key: &'static str,
		args: impl IntoMsgArgs,
		indices: impl IntoIterator<Item = usize>,
	) -> Self {
		Self {
			key,
			args: args.into_args(),
			key_args: indices.into_iter().collect(),
		}
	}

	pub fn render(&self, loc: Locale) -> String {
		let arg_strs: Vec<String> = self
			.args
			.iter()
			.enumerate()
			.map(|(i, s)| {
				if self.key_args.contains(&i) {
					let translated = t(s, loc);
					if !translated.is_empty() {
						translated.to_string()
					} else {
						s.clone()
					}
				} else {
					s.clone()
				}
			})
			.collect();
		let arg_refs: Vec<&str> = arg_strs.iter().map(|s| s.as_str()).collect();
		tf(self.key, loc, &arg_refs)
	}
}

impl std::fmt::Display for Msg {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.render(Locale::ZhTw))
	}
}

pub fn tf<T: std::fmt::Display>(key: &str, loc: Locale, args: &[T]) -> String {
	let raw = t(key, loc);
	if raw.is_empty() {
		if args.is_empty() {
			return key.to_string();
		}
		let mut res = key.to_string();
		res.push_str(" (");
		for (i, a) in args.iter().enumerate() {
			if i > 0 {
				res.push_str(", ");
			}
			res.push_str(&a.to_string());
		}
		res.push(')');
		return res;
	}
	let mut res = String::new();
	let mut arg_iter = args.iter();
	let mut chars = raw.chars().peekable();
	while let Some(c) = chars.next() {
		if c == '{' {
			let mut num_str = String::new();
			let mut matched_end = false;
			while let Some(&next_c) = chars.peek() {
				if next_c == '}' {
					chars.next();
					matched_end = true;
					break;
				} else if next_c.is_ascii_digit() {
					num_str.push(next_c);
					chars.next();
				} else {
					break;
				}
			}
			if matched_end {
				if num_str.is_empty() {
					if let Some(arg) = arg_iter.next() {
						res.push_str(&arg.to_string());
					} else {
						res.push_str("{}");
					}
				} else if let Ok(idx) = num_str.parse::<usize>() {
					if let Some(arg) = args.get(idx) {
						res.push_str(&arg.to_string());
					} else {
						res.push('{');
						res.push_str(&num_str);
						res.push('}');
					}
				}
			} else {
				res.push(c);
				res.push_str(&num_str);
			}
		} else {
			res.push(c);
		}
	}
	// Extra args are ignored: a message may pass data its wording omits.
	res
}

pub fn t(key: &str, loc: Locale) -> &'static str {
	match loc {
		Locale::ZhTw => match key {
			"app_title" => "snip-sync 原生工作台",
			"prototype_tag" => "GPUI 原生切片原型",
			"repos_title" => "儲存庫列表",
			"tab_changes" => "變更清單",
			"tab_files" => "檔案總覽",
			"history_title" => "Commit 歷史",
			"preview_title" => "內容預覽",
			"btn_copy_cancel" => "取消複製",
			"btn_paste" => "貼上預覽",
			"tip_group_by_dir" => "依目錄分組",
			"btn_refresh" => "重新整理",
			"btn_discovery_continue" => "繼續搜尋",
			"btn_discovery_retry" => "重試搜尋",
			"discovery_incomplete" => "搜尋未完成",
			"discovery_limit_reached" => "已達搜尋上限",
			"discovery_cancelled" => "搜尋已取消",
			"discovery_timed_out" => "搜尋逾時",
			"discovery_not_run" => "尚未執行搜尋",
			"discovery_failed" => "搜尋失敗",
			"btn_add_repo" => "新增",
			"add_repo_placeholder" => "儲存庫路徑...",
			"repo_kind_worktree" => "工作樹",
			"repo_kind_submodule" => "子模組",
			"repo_kind_uninit_submodule" => "未初始化",
			"repo_detached" => "分離 HEAD",
			"repo_unborn" => "未建立分支",
			"btn_apply_paste" => "確認套用還原",
			"btn_cancel_paste" => "取消還原",
			"btn_toggle_lang" => "EN",
			"tag_staged" => "已暫存",
			"tag_unstaged" => "未暫存",
			"tag_untracked" => "未追蹤",
			"tag_conflict" => "衝突",
			"tag_nested_repo" => "巢狀儲存庫",
			"destination_label" => "還原目的目錄",
			"overwrite_label" => "允許覆寫既有檔案",
			"overwrite_off_default" => "(預設關閉)",
			"paste_preview_heading" => "剪貼簿貼上預覽",
			"paste_apply_busy" => "正在套用還原至目的目錄...",
			"paste_applied" => "貼上還原完成",
			"paste_cancelled" => "已取消貼上預覽",
			"paste_loading" => "正在讀取目的地以建立貼上預覽…",
			"paste_loading_refused" => "貼上預覽仍在建立中，尚不能套用",
			"changes_scanning" => "正在搜尋 Git 儲存庫…",
			"changes_loading" => "正在讀取變更…",
			"changes_no_repository" => "這個資料夾裡沒有 Git 儲存庫",
			"changes_no_match" => "沒有符合篩選條件的變更",
			"changes_clean_partial" => {
				"已找到的儲存庫沒有變更；部分資料夾尚未掃描（可繼續掃描）"
			}
			"clean_working_copy" => "目前沒有未提交的變更檔案。",
			"find_placeholder" => "在檔案中搜尋",
			"jump_placeholder" => "行號...",
			"btn_jump" => "跳至行",
			"btn_copy_view" => "複製預覽文字",
			"line_truncated_notice" => "長行僅截斷顯示；預覽原文仍保留。",
			"truncated_notice" => "[注意: 內容過長，已截斷顯示前 {} 行 / {}KB]",
			"binary_notice" => "[二進位檔案，無法顯示文字預覽 (包含 NUL 位元組)]",
			"nav_hint" => "↑/↓ 導覽, 空白鍵切換選取, Alt+1/2 切換儲存庫, Ctrl+C 複製, Ctrl+V 貼上預覽, Ctrl+Q 退出",
			"filter_all_refs" => "全部分支與標籤",
			"filter_head" => "HEAD",
			"tree_truncated" => "[已截斷顯示前 150 項]",
			"tree_read_error" => "[無法讀取目錄]",
			"dest_changed_error" => "目的地檔案在預覽建立後已被修改，已阻止套用以維護安全，請重新預覽。",
			"project" => "專案",
			"changes" => "變更",
			"git_log" => "Git 記錄",
			"log_tab" => "記錄",
			"hide" => "隱藏",
			"working_changes" => "工作目錄變更",
			"select_none" => "全不選",
			"clean" => "乾淨",
			"counts_tip" => "+ 已暫存　~ 未暫存　? 未追蹤　! 衝突",
			"refs_all" => "全部 refs",
			"refs_local" => "本機分支",
			"refs_remote" => "遠端追蹤分支",
			"refs_tags" => "標籤",
			// log pane (IJ-2a)
			"log_chip_branch" => "分支",
			"log_chip_user" => "使用者",
			"log_chip_date" => "日期",
			"log_chip_paths" => "路徑",
			"log_chip_repo" => "repo",
			"log_repo_all" => "全部儲存庫",
			"log_details_repo" => "儲存庫：{}",
			"status_log_cross_repo" => "所選提交分屬不同儲存庫；複製與比較只能在同一個儲存庫內進行",
			"status_log_tree_other_repo" => "此提交屬於 {}；請先在工具列選取該儲存庫再瀏覽其檔案樹",
			"status_log_merged_cap" => "合併記錄顯示最新的 {} 筆提交；以「儲存庫」篩選單一儲存庫可看更早的記錄",
			"log_user_me" => "我",
			"log_date_1d" => "過去 24 小時",
			"log_date_7d" => "過去 7 天",
			"log_date_30d" => "過去 30 天",
			"log_date_1y" => "過去 1 年",
			"log_paths_placeholder" => "路徑，例如 src/app",
			"log_paths_hint" => "輸入文字過濾 repo 與路徑；按 Enter 加入輸入的路徑",
			"log_paths_tree_empty" => "開啟專案工具視窗後即可在此勾選資料夾",
			"log_date_custom" => "自訂範圍",
			"log_date_from" => "起 YYYY-MM-DD",
			"log_date_to" => "迄 YYYY-MM-DD",
			"log_date_apply" => "套用",
			"log_date_invalid" => "日期須為 YYYY-MM-DD，且起日不晚於迄日",
			"log_branch_placeholder" => "分支或標籤",
			"log_head_current" => "HEAD（目前分支）",
			"log_clear_filter" => "清除篩選",
			"tip_log_refresh" => "重新整理",
			"tip_log_more" => "更多",
			"tip_log_regex" => "規則運算式",
			"tip_log_case" => "區分大小寫",
			"tip_branches_hide" => "隱藏分支窗格",
			"tip_branches_show" => "顯示分支窗格",
			"tip_expand_all" => "全部展開",
			"tip_collapse_all" => "全部收合",
			"log_more_details" => "顯示 commit 詳細資料",
			"log_more_branches" => "顯示分支窗格",
			"log_more_hash" => "顯示雜湊欄",
			"log_loading_more" => "正在載入更多 commit…",
			"log_dir_files" => "{} 個檔案",
			"log_details_empty" => "選取 commit 以檢視詳細資料",
			"log_selection_header" => "已選取 {} 個 commit",
			"status_selection_truncated" => "已選取 {} 個 commit，只列出前 {} 個的變更檔案",
			"log_details_on" => "於 {}",
			"log_details_committed" => "由 {} 提交於 {}",
			"log_details_in_branches" => "包含於 {} 個分支：{}",
			"log_details_show_all" => "顯示全部",
			"log_today" => "今天 {}",
			"log_yesterday" => "昨天 {}",
			// end log pane (IJ-2a)
			"log_loading" => "正在載入 commit 歷史…",
			"log_no_repository" => "這個資料夾裡沒有 Git 儲存庫",
			"log_failed_feeds" => "{0} 個儲存庫無法讀取：{1}",
			"empty_log" => "沒有 commit",
			"src_working_file" => "工作目錄檔案",
			"src_working_diff" => "工作目錄變更",
			"commit_files" => "此 commit 變更的檔案（唯讀）",
			"no_file" => "未開啟檔案",
			"paste_tab" => "貼上預覽",
			"apply" => "套用",
			"cancel" => "取消",
			"applying" => "套用中…",
			"paste_busy_refused" => "正在寫入目的地，無法中途取消或變更；請等待完成",
			"op_create" => "建立",
			"op_overwrite" => "覆寫",
			"op_delete" => "刪除",
			"op_skip" => "跳過",
			"op_excluded" => "已排除",
			"op_overwrite_pending" => "待允許覆寫",
			"op_refused" => "拒絕",
			"overwrite_toggle" => "覆寫",
			"reason_create" => "目的地不存在，將建立新檔",
			"reason_overwrite" => "將覆寫目的地既有檔案",
			"reason_exists" => "目的地已存在；覆寫預設關閉，將跳過",
			"reason_delete" => "將刪除目的地檔案，不寫入內容",
			"reason_delete_missing" => "目的地不存在，無需刪除",
			"reason_excluded" => "已排除，不會寫入",
			"reason_commit_excluded" => "已排除；commit 重播不能只套用部分檔案，需重新勾選才能套用",
			"reason_commit_overwrite_pending" => "目的地已有此檔；覆寫預設關閉，允許覆寫前無法套用",
			"reason_refused_renamed_from_dir" => "整個 commit 會被拒絕：重新命名的來源是目錄",
			"reason_refused_delete_dir" => "整個 commit 會被拒絕：要刪除的路徑是目錄",
			"reason_refused_dir_in_way" => "整個 commit 會被拒絕：目錄佔住了檔案位置",
			"reason_refused_file_in_way" => "整個 commit 會被拒絕：父目錄被檔案佔住",
			"reason_refusal_cause_renamed_from_dir" => "重新命名的來源是目錄",
			"reason_refusal_cause_delete_dir" => "要刪除的路徑是目錄",
			"reason_refusal_cause_dir_in_way" => "目錄佔住了檔案位置",
			"reason_refusal_cause_file_in_way" => "父目錄被檔案佔住",
			"paste_keys" => "Enter 套用 · Esc 取消 · ↑↓ 切換項目 · 空白鍵切換",
			"copy_from" => "複製來源",
			"selected" => "已選",
			"repos_count" => "個儲存庫",
			"no_repo" => "未選取儲存庫",
			"project_rev_title" => "專案 (commit {})",
			"changes_title" => "變更 ({})",
			"changes_repo_error" => "無法讀取變更：{}",
			"changes_unreadable" => "無法讀取的儲存庫",
			"changes_truncated" => "只顯示前 {} 個變更（共 {} 個）",
			"tip_ref_selector" => "切換分支 / 標籤 (目前: {})",
			"src_vs_first_parent_merge" => "與 first-parent 比較 (共 {} 個 parent)",
			"src_commit_file" => "Commit {} 的檔案",
			"src_commit_short" => "commit {}",
			"src_compare" => "比較 {}..{}",
			"compare_header" => "比較 {}..{}",
			"changed_files" => "{} 個變更檔案",
			"tip_compare" => "已選取 {} 個 commit 進行比較",
			"error_history" => "無法讀取 Git 歷史: {}",
            "error_selection_root" => "無法確認目前儲存庫位置，選取內容未變更。",
            "error_graph_budget" => "Git 圖形資料超過 16 MiB 上限，這次操作未套用。請縮小 refs 或搜尋範圍。",
			"group_staged" => "Staged",
			"group_unstaged" => "Unstaged",
			"group_conflicted" => "Conflicts",
			"src_staged_diff" => "已暫存變更",
			"src_unstaged_diff" => "未暫存變更",
			"btn_copy_commits" => "複製 Commits",
			"tip_copy_commits" => "複製選取的 commits 範圍至剪貼簿",
			"paste_commit_count" => "{} 個 commit",
			"paste_commit_count_refused" => "{} 個 commit（{} 個被拒絕）",
			"commit_header_refused_suffix" => "整個 commit 會被拒絕",
			"commit_no_message" => "（無訊息）",
			"commit_empty_note" => "無檔案異動，仍會建立空 commit",
			"commit_header_counts" => "{} 個檔案，{} 個不寫入",
			"reason_skip_generic" => "此檔案不會寫入",
			"reason_nc_binary" => "未複製：二進位檔，不寫入也不刪除",
			"reason_nc_non_utf8" => "未複製：非 UTF-8 編碼，不寫入也不刪除",
			"reason_nc_non_utf8_path" => "未複製：路徑不是 UTF-8，不寫入也不刪除",
			"reason_nc_unsupported" => "未複製：符號連結或子模組，不寫入也不刪除",
			"reason_nc_unreadable" => "未複製：來源端讀不到內容，不寫入也不刪除",
			"reason_skip_unsafe_path" => "路徑不安全，不寫入",
			"reason_skip_non_utf8_target" => "目的地現有檔案不是 UTF-8，不覆寫",
			"status_commits_copied" => "已複製 {} 個 commit（{} 個檔案、{} 字元）至剪貼簿",
			"status_commits_copied_skipped" => "已複製 {} 個 commit（{} 個檔案、{} 字元）至剪貼簿；{} 個檔案未複製：{}",
			"status_replay_done" => "Replay 完成：建立 {} 個 commit",
			"collapsed_n" => "+{} 個已收合節點",
			"status_repo_count" => "{} 個儲存庫 ({} 個異常)",
			"status_goto" => "已跳至第 {} 行",
			"status_goto_invalid" => "無效的行號 (總行數: {})",
			"status_copied_selection" => "已複製選取文字 ({} 個字元)",
			"status_clipboard_failed" => "剪貼簿操作失敗: {}",
			"status_clipboard_read_failed" => "讀取剪貼簿失敗: {}",
			"status_copied_preview" => "已複製預覽文字",
			"status_scanning" => "正在掃描儲存庫...",
			"status_repo_loading" => "正在載入儲存庫「{}」...",
			"status_repo_loaded" => "儲存庫「{}」載入完成（{} 個變更）",
			"status_repos_loaded" => "已載入 {} 個儲存庫",
			"status_repo_vanished" => "儲存庫「{}」已不存在，已關閉其內容",
			"change_not_utf8" => "檔名不是有效的 UTF-8，無法複製",
			"tree_name_not_utf8" => "檔名不是有效的 UTF-8，無法開啟或預覽",
			"status_no_repo" => "未選取儲存庫",
			"status_copy_empty" => "未選取檔案 (無法複製)",
			"status_copying" => "正在複製「{}」的選取檔案...",
			"status_copy_cancelled" => "已取消複製，剪貼簿未變更。",
			"status_copied" => "已從 {} 複製 {} 個檔案（{} 字元、{} 行，略過 {} 個）至剪貼簿",
			"status_copy_nothing" => "沒有可複製的檔案內容",
			"status_copied_limit" => "已從 {} 複製 {} 個檔案（{} 字元、{} 行，略過 {} 個）至剪貼簿；已達 {} 個檔案上限，其餘檔案未複製",
			"status_copy_nothing_skipped" => "沒有可複製的檔案內容：所選資料夾內的檔案都無法複製，已略過",
			"status_paste_preview" => "貼上預覽已就緒: {} 項變更",
			"paste_err_not_payload" => "剪貼簿內容不是有效的 snip-sync payload",
			"paste_err_nothing" => "剪貼簿 payload 不包含任何檔案",
			"paste_err_plan" => "無法建立貼上計畫: {}",
			"paste_err_destination" => "無法使用貼上目的地「{}」: {}",
			"paste_err_destination_not_dir" => "貼上目的地「{}」不是資料夾",
			"preview_memory_limit" => "預覽資料超過 32 MiB 上限，未載入這次變更。請縮小預覽範圍。",
			"stale_created" => "目的地檔案已在外部建立: {}",
			"stale_deleted" => "目的地檔案已在外部刪除: {}",
			"stale_modified" => "目的地檔案已在外部修改: {}",
			"error_repo_status" => "讀取儲存庫狀態失敗: {}",
			"error_repo_changes" => "讀取儲存庫「{}」變更失敗: {}",
			"error_binary" => "[二進位檔案，無法顯示文字預覽: {}]",
			"error_preview" => "預覽「{}」失敗: {}",
			"error_open_repo" => "開啟儲存庫失敗: {}",
			"error_payload" => "產生 payload 失敗: {}",
			"error_tree" => "讀取 commit 檔案樹失敗: {}",
			"status_loading" => "載入中...",
			"tree_rows_capped" => "[超過上限: 僅顯示前 {} 個項目]",
			"tree_dir_truncated" => "[目錄項目過多，已截斷顯示前 {} 項]",
			"status_no_changes" => "此 commit 沒有檔案變更",
			"error_not_utf8" => "[非 UTF-8 文字檔案: {}]",
			"error_too_large" => "[檔案「{}」過大 ({} 位元組)，超出預覽上限]",
			"repo_error_short" => "異常",
			"btn_back_to_working" => "返回工作目錄",
			"btn_browse_tree" => "瀏覽檔案樹",
			"diff_inline" => "行內",
			"diff_side" => "並排",
			"empty_project" => "此目錄沒有檔案",
			"goto_placeholder" => "行號...",
			"log_search_placeholder" => "文字或雜湊",
			"selector_empty" => "無相符項目",
			"selector_filter_placeholder" => "輸入文字進行篩選...",
			"selector_ref_title" => "分支與標籤",
			"selector_repo_title" => "儲存庫列表",
			"src_vs_empty_tree" => "與空樹比較 (根 commit)",
			"src_vs_first_parent" => "與 first-parent 比較",
			"status_history_loaded" => "已載入 {} 筆 commit",
			"status_graph_fallback" => "已載入 {} 筆 commit；同時進行的分支太多，本頁改以清單顯示，不畫線圖",
			"status_commit_files_truncated" => "此 commit 變更 {} 個檔案，只列出前 {} 個",
			"status_commit_files_truncated_min" => "此選取至少變更 {} 個檔案，只列出前 {} 個",
			"status_paste_done" => "還原完成：建立 {}、覆寫 {}、跳過 {}、刪除 {}、失敗 {}",
			"status_paste_partial" => "還原完成：建立 {}、覆寫 {}、跳過 {}、刪除 {}，{} 個檔案失敗：{}",
			"submodule" => "子模組",
			"tip_browse_tree" => "瀏覽此 commit 的完整檔案樹（唯讀）",
			"tip_changes" => "變更工具視窗 (Alt+0)",
			"tip_compare_disabled" => "按住 Shift 點選多個 commit 即可進行比較",
			"tip_copy_view" => "複製目前預覽文字至剪貼簿",
			"tip_find_next" => "下一處相符項目 (Enter / F3)",
			"tip_find_prev" => "上一處相符項目 (Shift+Enter / Shift+F3)",
			"tip_git_log" => "Git 記錄工具視窗 (Alt+9)",
			"tip_head" => "跳至 HEAD commit",
			"tip_project" => "專案工具視窗 (Alt+1)",
			"tip_repo_selector" => "切換儲存庫 (Alt+Shift+R)",
			"mapping_required" => "請先為每個來源前綴選擇目的地，套用前不會寫入",
			"mapping_prefix" => "來源 {}",
			"mapping_unresolved" => "尚未選擇目的地",
			"mapping_keep" => "留在主要目錄",
			"mapping_unknown_prefix" => "未知的來源前綴 {}",
			"mapping_unknown_dest" => "目的地不在可選清單：{}",
			"commit_subset_rejected" => "提交重放必須套用整段內容與中繼資料。取消任一項會在寫入前拒絕，不會只寫其餘檔案",
			"commit_overwrite_required" => "目的地已有檔案，覆寫預設關閉。請允許覆寫後再套用，否則不會寫入",
			"commit_will_be_refused" => "第 {} 個 commit「{}」會被拒絕，重播將在此停止",
			"commit_replay_refused" => "沒有建立任何 commit；第 {} 個 commit「{}」被拒絕：{}",
			"commit_replay_partial" => "重放中途失敗。已建立且不會丟棄的提交：{}。錯誤：{}",
			"commit_replay_partial_refused" => "重放中途停止。已建立且不會丟棄的提交：{}。第 {} 個 commit 被拒絕：{}：{}",
			"commit_replay_done" => "提交重放完成：{}",
			"commit_whole_note" => "這是整段提交重放。取消任一檔會拒絕整段寫入；覆寫既有檔案必須另外確認",
			"tip_add_repo" => "新增儲存庫…",
			"hint_switch_repo" => "切換儲存庫",
			"hint_switch_ref" => "切換分支 / 標籤",
			"workspace_close" => "關閉工作區",
			"workspace_open" => "輸入路徑…",
			"workspace_open_folder" => "開啟資料夾…",
			"workspace_recent" => "最近開啟",
			"workspace_path_placeholder" => "工作區資料夾路徑…",
			"workspace_open_confirm" => "開啟",
			"workspace_closed" => "未開啟工作區。開啟一個 Git 儲存庫，或內含多個儲存庫的資料夾。",
			"workspace_not_open" => "未開啟工作區，無法貼上。請先開啟工作區。",
			"workspace_none" => "未開啟工作區",
			"workspace_busy_applying" => "正在套用變更，已拒絕關閉、開啟與結束，以免寫入中斷。",
			"workspace_draining" => "正在取消讀取並等候 Git 結束…",
			"workspace_drain_timeout" => "無法在時限內確認工作已結束，視窗保持開啟以便復原。",
			"workspace_drain_leaked" => "Git 子程序未能確認結束，視窗保持開啟以便復原。",
			"remote_section" => "遠端主機（SSH）",
			"remote_no_hosts" => "~/.ssh/config 裡沒有主機",
			"remote_path_placeholder" => "遠端資料夾路徑，例如 ~/project",
			"remote_open_path" => "開啟",
			"remote_opening" => "開啟中…",
			"remote_path_missing" => "請輸入遠端資料夾路徑",
			"remote_host_missing" => "~/.ssh/config 裡已經沒有主機 {}",
			"remote_loading" => "連線中…",
			"remote_recent" => "最近開啟",
			"remote_opened" => "已開啟遠端工作區 {}",
			"remote_open_failed" => "無法開啟遠端工作區：{}",
			"remote_worker_too_old" => "{0} 上的 snip-sync 版本太舊（協定 {1}），不支援 Git 檢視；請在那台機器更新",
			"remote_scan_failed" => "無法讀取遠端的 Git 儲存庫：{0}",
			"remote_scan_incomplete" => "找到 {} 個儲存庫，遠端掃描未完成；重新整理可重掃",
			"remote_refs_too_large" => "遠端參照資料過大",
			"remote_unsupported" => "遠端工作區還不支援這個操作（貼上即將支援）",
			"workspace_opening" => "正在開啟工作區 {}…",
			"workspace_bad_path" => "找不到工作區資料夾：{}",
			"lifecycle_jobs" => "工作 {}",
			"tip_workspace_menu" => "關閉 Ctrl+Shift+W，開啟 Ctrl+Shift+O",
			// chrome (IJ-2c)
			"menu_copy_path" => "複製路徑",
			"menu_copy_relative_path" => "複製相對路徑",
			"menu_copy_files" => "複製",
			"menu_show_diff" => "顯示差異",
			"menu_copy_revision" => "複製修訂版號",
			"menu_go_parent" => "前往父 commit",
			"menu_go_child" => "前往子 commit",
			"menu_browse_tree" => "瀏覽此修訂版的檔案",
			"status_text_copied" => "已複製: {}",
			"status_repo_count_ok" => "{} 個儲存庫",
			"tip_language" => "切換語言",
			"tip_vcs_branch" => "目前分支",
			"speed_search_none" => "沒有符合項目",
			"menu_reveal_finder" => "在 Finder 中顯示",
			"menu_reveal_explorer" => "在檔案總管中顯示",
			"menu_reveal_files" => "在檔案管理員中開啟所在資料夾",
			"status_reveal_failed" => "無法開啟檔案管理員: {}",
			"menu_close_tab" => "關閉",
			"menu_close_other_tabs" => "關閉其他分頁",
			"menu_close_all_tabs" => "關閉所有分頁",
			// editor (IJ-2b)
			"diff_fold" => "⋯ {} 行未變更",
			"tip_diff_side" => "切換為並排檢視",
			"tip_diff_unified" => "切換為統一檢視",
			"tip_next_diff" => "下一處差異 (F7)",
			"tip_prev_diff" => "上一處差異 (Shift+F7)",
			"tip_match_case" => "區分大小寫",
			"tip_regex" => "規則運算式",
			"tip_find_close" => "關閉 (Esc)",
			"tip_goto" => "跳至行號 (Ctrl+G)",
			"tip_close_tab" => "關閉分頁",
			"tip_preview_tab" => "預覽分頁：開啟下一個檔案時會取代它，按兩下分頁可固定",
			"tip_expand_folds" => "展開所有未變更的行",
			"diff_fold_end" => "⋯ 未變更的行直到檔案結尾",
			"status_fold_failed" => "無法展開未變更的行：{}",
			"status_fold_stale" => "檔案在差異產生後已變更，請重新整理後再展開",
			"status_fold_too_large" => "展開後超過預覽上限，保留摺疊",
			_ => "",
		},
		Locale::En => match key {
			"app_title" => "snip-sync Native Workbench",
			"prototype_tag" => "GPUI Native Prototype",
			"repos_title" => "Repositories",
			"tab_changes" => "Git Changes",
			"tab_files" => "File Explorer",
			"history_title" => "Commit History",
			"preview_title" => "Content Preview",
			"btn_copy_cancel" => "Cancel copy",
			"btn_paste" => "Paste Preview",
			"tip_group_by_dir" => "Group by Directory",
			"btn_refresh" => "Refresh",
			"btn_discovery_continue" => "Continue Discovery",
			"btn_discovery_retry" => "Retry Discovery",
			"discovery_incomplete" => "discovery incomplete",
			"discovery_limit_reached" => "discovery limit reached",
			"discovery_cancelled" => "discovery cancelled",
			"discovery_timed_out" => "discovery timed out",
			"discovery_not_run" => "discovery not run",
			"discovery_failed" => "discovery failed",
			"btn_add_repo" => "Add",
			"add_repo_placeholder" => "Repository path...",
			"repo_kind_worktree" => "worktree",
			"repo_kind_submodule" => "submodule",
			"repo_kind_uninit_submodule" => "uninitialized",
			"repo_detached" => "detached",
			"repo_unborn" => "unborn",
			"btn_apply_paste" => "Apply Restore",
			"btn_cancel_paste" => "Cancel Restore",
			"btn_toggle_lang" => "繁中",
			"tag_staged" => "staged",
			"tag_unstaged" => "unstaged",
			"tag_untracked" => "untracked",
			"tag_conflict" => "conflict",
			"tag_nested_repo" => "nested repo",
			"destination_label" => "Restore Destination",
			"overwrite_label" => "Allow overwriting existing files",
			"overwrite_off_default" => "(Off by default)",
			"paste_preview_heading" => "Paste Restore Preview",
			"paste_apply_busy" => "Applying restore to destination...",
			"paste_applied" => "Paste restore completed",
			"paste_cancelled" => "Paste preview cancelled",
			"paste_loading" => "Reading the destination to build the paste preview…",
			"paste_loading_refused" => "The paste preview is still being built; it cannot be applied yet",
			"changes_scanning" => "Scanning for Git repositories…",
			"changes_loading" => "Loading changes…",
			"changes_no_repository" => "No Git repository in this folder",
			"changes_no_match" => "No changes match the filter",
			"changes_clean_partial" => {
				"No changes in the repositories found; some folders were not scanned (use continue scanning)"
			}
			"clean_working_copy" => "Working tree is clean.",
			"find_placeholder" => "Find in file",
			"jump_placeholder" => "Line #...",
			"btn_jump" => "Jump",
			"btn_copy_view" => "Copy Preview Text",
			"line_truncated_notice" => "Long lines are clipped for display; preview text is retained.",
			"truncated_notice" => "[Notice: Content too long, truncated to first {} lines / {}KB]",
			"binary_notice" => "[Binary file, text preview unavailable (contains NUL bytes)]",
			"nav_hint" => "↑/↓ Navigate, Space Toggle, Alt+1/2 Switch Repo, Ctrl+C Copy, Ctrl+V Paste Preview, Ctrl+Q Quit",
			"filter_all_refs" => "All Branches & Tags",
			"filter_head" => "HEAD",
			"tree_truncated" => "[Truncated: first 150 entries]",
			"tree_read_error" => "[Cannot read directory]",
			"dest_changed_error" => "Destination files changed after preview was built. Apply was blocked for safety; please regenerate preview.",
			"project" => "Project",
			"changes" => "Changes",
			"git_log" => "Git Log",
			"log_tab" => "Log",
			"hide" => "Hide",
			"working_changes" => "Working changes",
			"select_none" => "None",
			"clean" => "clean",
			"counts_tip" => "+ staged  ~ unstaged  ? untracked  ! conflicts",
			"refs_all" => "All refs",
			"refs_local" => "Local",
			"refs_remote" => "Remote",
			"refs_tags" => "Tags",
			// log pane (IJ-2a)
			"log_chip_branch" => "Branch",
			"log_chip_user" => "User",
			"log_chip_date" => "Date",
			"log_chip_paths" => "Paths",
			"log_chip_repo" => "Repo",
			"log_repo_all" => "All repositories",
			"log_details_repo" => "Repository: {}",
			"status_log_cross_repo" => "The selected commits belong to different repositories; copy and compare work within one repository",
			"status_log_tree_other_repo" => "This commit belongs to {}; select that repository in the toolbar to browse its tree",
			"status_log_merged_cap" => "The merged log shows the latest {} commits; filter by Repository to see older ones",
			"log_user_me" => "me",
			"log_date_1d" => "Last 24 hours",
			"log_date_7d" => "Last 7 days",
			"log_date_30d" => "Last 30 days",
			"log_date_1y" => "Last year",
			"log_paths_placeholder" => "Path, e.g. src/app",
			"log_paths_hint" => "Type to filter repos and paths; Enter adds the typed path",
			"log_paths_tree_empty" => "Open the Project tool window to pick folders here",
			"log_date_custom" => "Custom range",
			"log_date_from" => "From YYYY-MM-DD",
			"log_date_to" => "To YYYY-MM-DD",
			"log_date_apply" => "Apply",
			"log_date_invalid" => "Use YYYY-MM-DD, with the start not after the end",
			"log_branch_placeholder" => "Branch or tag",
			"log_head_current" => "HEAD (Current Branch)",
			"log_clear_filter" => "Clear filter",
			"tip_log_refresh" => "Refresh",
			"tip_log_more" => "More",
			"tip_log_regex" => "Regex",
			"tip_log_case" => "Match case",
			"tip_branches_hide" => "Hide branches",
			"tip_branches_show" => "Show branches",
			"tip_expand_all" => "Expand all",
			"tip_collapse_all" => "Collapse all",
			"log_more_details" => "Show commit details",
			"log_more_branches" => "Show branches",
			"log_more_hash" => "Show hash column",
			"log_loading_more" => "Loading more commits…",
			"log_dir_files" => "{} files",
			"log_details_empty" => "Select a commit to see its details",
			"log_selection_header" => "{} commits selected",
			"status_selection_truncated" => "{} commits selected; listing the changed files of the first {}",
			"log_details_on" => "on {}",
			"log_details_committed" => "committed by {} on {}",
			"log_details_in_branches" => "In {} branches: {}",
			"log_details_show_all" => "Show all",
			"log_today" => "Today {}",
			"log_yesterday" => "Yesterday {}",
			// end log pane (IJ-2a)
			"log_loading" => "Loading commit history…",
			"log_no_repository" => "No Git repository in this folder",
			"log_failed_feeds" => "{0} repository(ies) could not be read: {1}",
			"empty_log" => "No commits",
			"src_working_file" => "Working tree file",
			"src_working_diff" => "Working changes",
			"commit_files" => "Files changed in this commit (read-only)",
			"no_file" => "No file open",
			"paste_tab" => "Paste Preview",
			"apply" => "Apply",
			"cancel" => "Cancel",
			"applying" => "Applying…",
			"paste_busy_refused" => "Writing to the destination; it cannot be cancelled or changed midway. Wait for it to finish.",
			"op_create" => "Create",
			"op_overwrite" => "Overwrite",
			"op_delete" => "Delete",
			"op_skip" => "Skip",
			"op_excluded" => "Excluded",
			"op_overwrite_pending" => "Overwrite pending",
			"op_refused" => "Refused",
			"overwrite_toggle" => "Overwrite",
			"reason_create" => "Not present at destination; will be created",
			"reason_overwrite" => "Will overwrite the existing destination file",
			"reason_exists" => "Exists at destination; overwrite is off, will skip",
			"reason_delete" => "Will delete the destination file",
			"reason_delete_missing" => "Not present at destination; nothing to delete",
			"reason_excluded" => "Excluded; nothing will be written",
			"reason_commit_excluded" => "Excluded; a commit replay cannot apply a subset of files, so re-select it to apply",
			"reason_commit_overwrite_pending" => "The file exists at the destination and overwrite is off; Apply is blocked until overwrite is allowed",
			"reason_refused_renamed_from_dir" => "Whole commit will be refused: the renamed-from path is a directory",
			"reason_refused_delete_dir" => "Whole commit will be refused: the path to delete is a directory",
			"reason_refused_dir_in_way" => "Whole commit will be refused: a directory is in the way of the file",
			"reason_refused_file_in_way" => "Whole commit will be refused: a file is in the way of its parent directory",
			"reason_refusal_cause_renamed_from_dir" => "the renamed-from path is a directory",
			"reason_refusal_cause_delete_dir" => "the path to delete is a directory",
			"reason_refusal_cause_dir_in_way" => "a directory is in the way of the file",
			"reason_refusal_cause_file_in_way" => "a file is in the way of its parent directory",
			"paste_keys" => "Enter apply · Esc cancel · ↑↓ item · Space toggle",
			"copy_from" => "Copy from",
			"selected" => "selected",
			"repos_count" => "repos",
			"no_repo" => "No repository",
			"project_rev_title" => "Project (commit {})",
			"changes_title" => "Changes ({})",
			"changes_repo_error" => "Cannot read changes: {}",
			"changes_unreadable" => "Unreadable repositories",
			"changes_truncated" => "Showing the first {} of {} changes",
			"tip_ref_selector" => "Switch branch / tag (current: {})",
			"src_vs_first_parent_merge" => "Compare vs first-parent ({} parents)",
			"src_commit_file" => "File in commit {}",
			"src_commit_short" => "commit {}",
			"src_compare" => "Compare {}..{}",
			"compare_header" => "Compare {}..{}",
			"changed_files" => "{} changed files",
			"tip_compare" => "{} commits selected for compare",
			"error_history" => "Failed to read Git history: {}",
            "error_selection_root" => "The current repository path could not be verified; the selection was not changed.",
            "error_graph_budget" => "Git graph exceeds the 16 MiB data budget; this operation was not applied. Narrow the refs or search.",
			"group_staged" => "Staged",
			"group_unstaged" => "Unstaged",
			"group_conflicted" => "Conflicts",
			"src_staged_diff" => "Staged changes",
			"src_unstaged_diff" => "Unstaged changes",
			"btn_copy_commits" => "Copy Commits",
			"tip_copy_commits" => "Copy selected commit range to clipboard",
			"paste_commit_count" => "{} commit(s)",
			"paste_commit_count_refused" => "{} commit(s) ({} refused)",
			"commit_header_refused_suffix" => "whole commit will be refused",
			"commit_no_message" => "(no message)",
			"commit_empty_note" => "No file changes; an empty commit will still be created",
			"commit_header_counts" => "{} file(s), {} not written",
			"reason_skip_generic" => "This file will not be written",
			"reason_nc_binary" => "Not copied: binary file, neither written nor deleted",
			"reason_nc_non_utf8" => "Not copied: not UTF-8, neither written nor deleted",
			"reason_nc_non_utf8_path" => "Not copied: path is not UTF-8, neither written nor deleted",
			"reason_nc_unsupported" => "Not copied: symlink or submodule, neither written nor deleted",
			"reason_nc_unreadable" => "Not copied: source content could not be read, neither written nor deleted",
			"reason_skip_unsafe_path" => "Unsafe path; not written",
			"reason_skip_non_utf8_target" => "The existing file at the destination is not UTF-8; not overwritten",
			"status_commits_copied" => "Copied {} commit(s) ({} files, {} chars) to clipboard",
			"status_commits_copied_skipped" => "Copied {} commit(s) ({} files, {} chars) to clipboard; {} file(s) not copied: {}",
			"status_replay_done" => "Replay complete: {} commit(s) created",
			"collapsed_n" => "+{} collapsed nodes",
			"status_repo_count" => "{} repos ({} errors)",
			"status_goto" => "Jumped to line {}",
			"status_goto_invalid" => "Invalid line number (total lines: {})",
			"status_copied_selection" => "Copied selection ({} chars)",
			"status_clipboard_failed" => "Clipboard operation failed: {}",
			"status_clipboard_read_failed" => "Failed to read clipboard: {}",
			"status_copied_preview" => "Copied preview text",
			"status_scanning" => "Scanning repositories...",
			"status_repo_loading" => "Loading repository '{}'...",
			"status_repo_loaded" => "Repository '{}' loaded ({} changes)",
			"status_repos_loaded" => "Loaded {} repositories",
			"status_repo_vanished" => "Repository '{}' no longer exists; its view was closed",
			"change_not_utf8" => "File name is not valid UTF-8 and cannot be copied",
			"tree_name_not_utf8" => "File name is not valid UTF-8 and cannot be opened or previewed",
			"status_no_repo" => "No repository selected",
			"status_copy_empty" => "No selection (cannot copy)",
			"status_copying" => "Copying selection from '{}'...",
			"status_copy_cancelled" => "Copy cancelled. The clipboard was not changed.",
			"status_copied" => "Copied from {}: {} files ({} chars, {} lines, {} skipped) to clipboard",
			"status_copy_nothing" => "No file content to copy",
			"status_copied_limit" => "Copied from {}: {} files ({} chars, {} lines, {} skipped) to clipboard; reached the {}-file limit, the rest were not copied",
			"status_copy_nothing_skipped" => "No file content to copy: every file in the selected folders was skipped",
			"status_paste_preview" => "Paste preview ready: {} items",
			"paste_err_not_payload" => "Clipboard content is not a valid snip-sync payload",
			"paste_err_nothing" => "Clipboard payload contains no files",
			"paste_err_plan" => "Could not build the paste plan: {}",
			"paste_err_destination" => "Cannot use paste destination \"{}\": {}",
			"paste_err_destination_not_dir" => "Paste destination \"{}\" is not a directory",
			"preview_memory_limit" => "Preview data exceeds the 32 MiB limit. This change was not loaded. Choose a smaller preview.",
			"stale_created" => "Destination file was created externally: {}",
			"stale_deleted" => "Destination file was deleted externally: {}",
			"stale_modified" => "Destination file was modified externally: {}",
			"error_repo_status" => "Failed to read repository status: {}",
			"error_repo_changes" => "Failed to read changes for '{}': {}",
			"error_binary" => "[Binary file, cannot display text preview: {}]",
			"error_preview" => "Failed to preview '{}': {}",
			"error_open_repo" => "Failed to open repository: {}",
			"error_payload" => "Failed to generate payload: {}",
			"error_tree" => "Failed to read commit tree: {}",
			"status_loading" => "Loading...",
			"tree_rows_capped" => "[Capped: showing first {} items]",
			"tree_dir_truncated" => "[Directory truncated to first {} entries]",
			"status_no_changes" => "No file changes in this commit",
			"error_not_utf8" => "[Non-UTF-8 text file: {}]",
			"error_too_large" => "[File '{}' is too large ({} bytes), exceeds preview limit]",
			"repo_error_short" => "error",
			"btn_back_to_working" => "Back to Working",
			"btn_browse_tree" => "Browse Tree",
			"diff_inline" => "Inline",
			"diff_side" => "Side-by-side",
			"empty_project" => "No files in this directory",
			"goto_placeholder" => "Line #...",
			"log_search_placeholder" => "Text or hash",
			"selector_empty" => "No matches found",
			"selector_filter_placeholder" => "Type to filter...",
			"selector_ref_title" => "Branches & Tags",
			"selector_repo_title" => "Repositories",
			"src_vs_empty_tree" => "Compare vs empty tree (root commit)",
			"src_vs_first_parent" => "Compare vs first-parent",
			"status_history_loaded" => "Loaded {} commits",
			"status_graph_fallback" => "Loaded {} commits; too many concurrent branches, so this page is a plain list without graph lines",
			"status_commit_files_truncated" => "This commit changes {} files; showing the first {}",
			"status_commit_files_truncated_min" => "This selection changes at least {} files; showing the first {}",
			"status_paste_done" => "Restore done: created {}, overwritten {}, skipped {}, deleted {}, errors {}",
			"status_paste_partial" => "Restore done: created {}, overwritten {}, skipped {}, deleted {}; {} files failed: {}",
			"submodule" => "Submodule",
			"tip_browse_tree" => "Browse full file tree of this commit (read-only)",
			"tip_changes" => "Changes tool window (Alt+0)",
			"tip_compare_disabled" => "Shift-click multiple commits to compare",
			"tip_copy_view" => "Copy preview text to clipboard",
			"tip_find_next" => "Next match (Enter / F3)",
			"tip_find_prev" => "Previous match (Shift+Enter / Shift+F3)",
			"tip_git_log" => "Git Log tool window (Alt+9)",
			"tip_head" => "Jump to HEAD commit",
			"tip_project" => "Project tool window (Alt+1)",
			"tip_repo_selector" => "Switch repository (Alt+Shift+R)",
			"mapping_required" => "Choose a destination for every source prefix. Nothing is written until you do",
			"mapping_prefix" => "Source {}",
			"mapping_unresolved" => "No destination chosen",
			"mapping_keep" => "Keep under primary",
			"mapping_unknown_prefix" => "Unknown source prefix {}",
			"mapping_unknown_dest" => "Destination is not a candidate: {}",
			"commit_subset_rejected" => "Commit replay keeps the whole commit content and metadata. Unchecking any item rejects the replay before any write",
			"commit_overwrite_required" => "A destination file already exists and overwrite starts off. Allow overwrite before applying; nothing is written until you do",
			"commit_will_be_refused" => "Commit #{} \"{}\" will be refused; replay will stop there",
			"commit_replay_refused" => "No commit was created; commit #{} \"{}\" was refused: {}",
			"commit_replay_partial" => "Replay stopped midway. Commits already created are kept: {}. Error: {}",
			"commit_replay_partial_refused" => "Replay stopped midway. Commits already created are kept: {}. Commit #{} was refused: {}: {}",
			"commit_replay_done" => "Commit replay finished: {}",
			"commit_whole_note" => "This replays the whole commit. Unchecking any file rejects the entire write. Overwriting existing files needs a separate confirmation",
			"tip_add_repo" => "Add repository…",
			"hint_switch_repo" => "Switch repository",
			"hint_switch_ref" => "Switch branch / tag",
			"workspace_close" => "Close workspace",
			"workspace_open" => "Enter path…",
			"workspace_open_folder" => "Open Folder…",
			"workspace_recent" => "Recent",
			"workspace_path_placeholder" => "Workspace folder path…",
			"workspace_open_confirm" => "Open",
			"workspace_closed" => "No workspace open. Open a Git repository, or a folder that contains several.",
			"workspace_not_open" => "No workspace is open. Open a workspace before pasting.",
			"workspace_none" => "No workspace",
			"workspace_busy_applying" => "A change is being applied. Close, open, and quit are refused so the write is not interrupted.",
			"workspace_draining" => "Cancelling reads and waiting for Git to finish…",
			"workspace_drain_timeout" => "Work could not be confirmed finished in time. The window stays open so you can recover.",
			"workspace_drain_leaked" => "A Git child could not be confirmed gone. The window stays open so you can recover.",
			"remote_section" => "Remote hosts (SSH)",
			"remote_no_hosts" => "No host in ~/.ssh/config",
			"remote_path_placeholder" => "Remote folder path, e.g. ~/project",
			"remote_open_path" => "Open",
			"remote_opening" => "Opening…",
			"remote_path_missing" => "Enter a remote folder path",
			"remote_host_missing" => "~/.ssh/config no longer names host {}",
			"remote_loading" => "Connecting…",
			"remote_recent" => "Recent",
			"remote_opened" => "Opened remote workspace {}",
			"remote_open_failed" => "Cannot open the remote workspace: {}",
			"remote_worker_too_old" => "snip-sync on {0} is outdated (protocol {1}) and has no Git view; update it on that machine",
			"remote_scan_failed" => "Failed to read remote Git repositories: {0}",
			"remote_scan_incomplete" => "Found {} repositories; the remote scan is incomplete. Refresh to rescan",
			"remote_refs_too_large" => "Remote references too large",
			"remote_unsupported" => "A remote workspace does not support this yet (paste is coming)",
			"workspace_opening" => "Opening workspace {}…",
			"workspace_bad_path" => "Workspace folder was not found: {}",
			"lifecycle_jobs" => "jobs {}",
			"tip_workspace_menu" => "Close Ctrl+Shift+W, open Ctrl+Shift+O",
			// chrome (IJ-2c)
			"menu_copy_path" => "Copy Path",
			"menu_copy_relative_path" => "Copy Relative Path",
			"menu_copy_files" => "Copy",
			"menu_show_diff" => "Show Diff",
			"menu_copy_revision" => "Copy Revision Number",
			"menu_go_parent" => "Go to Parent Commit",
			"menu_go_child" => "Go to Child Commit",
			"menu_browse_tree" => "Browse Files at Revision",
			"status_text_copied" => "Copied: {}",
			"status_repo_count_ok" => "{} repositories",
			"tip_language" => "Switch language",
			"tip_vcs_branch" => "Current branch",
			"speed_search_none" => "No matches",
			"menu_reveal_finder" => "Reveal in Finder",
			"menu_reveal_explorer" => "Show in Explorer",
			"menu_reveal_files" => "Open Containing Folder",
			"status_reveal_failed" => "Could not open the file manager: {}",
			"menu_close_tab" => "Close",
			"menu_close_other_tabs" => "Close Other Tabs",
			"menu_close_all_tabs" => "Close All Tabs",
			// editor (IJ-2b)
			"diff_fold" => "⋯ {} unchanged lines",
			"tip_diff_side" => "Switch to side-by-side viewer",
			"tip_diff_unified" => "Switch to unified viewer",
			"tip_next_diff" => "Next difference (F7)",
			"tip_prev_diff" => "Previous difference (Shift+F7)",
			"tip_match_case" => "Match case",
			"tip_regex" => "Regex",
			"tip_find_close" => "Close (Esc)",
			"tip_goto" => "Go to line (Ctrl+G)",
			"tip_close_tab" => "Close tab",
			"tip_preview_tab" => "Preview tab: the next file you open replaces it; double-click the tab to keep it",
			"tip_expand_folds" => "Expand all unchanged lines",
			"diff_fold_end" => "⋯ unchanged lines to end of file",
			"status_fold_failed" => "Could not expand unchanged lines: {}",
			"status_fold_stale" => "The file changed since this diff was made; refresh to expand",
			"status_fold_too_large" => "Expanding would exceed the preview limit; kept folded",
			_ => "",
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_status_history_loaded_renders_without_raw_key() {
		let msg = Msg::new("status_history_loaded", ["4".to_string()]);
		let zh = msg.render(Locale::ZhTw);
		assert_eq!(zh, "已載入 4 筆 commit");
		let en = msg.render(Locale::En);
		assert_eq!(en, "Loaded 4 commits");
	}

	#[test]
	fn unused_args_are_not_printed() {
		let msg =
			Msg::new("status_repos_loaded", ["1".to_string(), "0".to_string()]);
		assert_eq!(msg.render(Locale::ZhTw), "已載入 1 個儲存庫");
	}

	#[test]
	fn paste_err_destination_names_path_and_reason() {
		let msg = Msg::new(
			"paste_err_destination",
			["/t/newdir/x".to_string(), "Not a directory".to_string()],
		);
		for loc in [Locale::ZhTw, Locale::En] {
			let s = msg.render(loc);
			assert!(!s.starts_with("paste_err_"), "raw key leaked: {s}");
			assert!(s.contains("/t/newdir/x") && s.contains("Not a directory"));
		}
	}

	/// Every `Msg::new("key", ..)` in this crate must have both translations,
	/// or the status bar shows the raw key.
	#[test]
	fn every_msg_key_used_in_source_is_translated() {
		let mut keys = std::collections::BTreeSet::new();
		let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
		let mut stack = vec![dir];
		while let Some(d) = stack.pop() {
			for e in std::fs::read_dir(d).unwrap() {
				let p = e.unwrap().path();
				if p.is_dir() {
					stack.push(p);
				} else if p.extension().is_some_and(|x| x == "rs")
					&& p.file_name().is_some_and(|n| n != "i18n.rs")
				{
					let src = std::fs::read_to_string(&p).unwrap();
					let mut rest = src.as_str();
					while let Some(at) = rest.find("Msg::new(") {
						rest = rest[at + 9..].trim_start();
						let key = rest
							.strip_prefix('"')
							.and_then(|r| r.split('"').next());
						if let Some(key) = key {
							keys.insert(key.to_string());
						}
					}
				}
			}
		}
		assert!(keys.len() > 20, "scan found too few keys: {keys:?}");
		for key in keys {
			assert!(
				!t(&key, Locale::ZhTw).is_empty(),
				"Missing ZhTw key: {key}"
			);
			assert!(!t(&key, Locale::En).is_empty(), "Missing En key: {key}");
		}
	}

	#[test]
	fn test_i18n_keys_parity() {
		let test_keys = [
			"btn_back_to_working",
			"btn_browse_tree",
			"diff_inline",
			"diff_side",
			"discovery_incomplete",
			"discovery_limit_reached",
			"discovery_cancelled",
			"discovery_timed_out",
			"discovery_not_run",
			"discovery_failed",
			"empty_project",
			"goto_placeholder",
			"log_search_placeholder",
			"selector_empty",
			"selector_filter_placeholder",
			"selector_ref_title",
			"selector_repo_title",
			"src_vs_empty_tree",
			"src_vs_first_parent",
			"status_commit_files_truncated",
			"status_commit_files_truncated_min",
			"status_history_loaded",
			"status_paste_done",
			"status_paste_partial",
			"submodule",
			"tip_browse_tree",
			"tip_changes",
			"tip_compare_disabled",
			"tip_copy_view",
			"tip_find_next",
			"tip_find_prev",
			"tip_git_log",
			"tip_head",
			"tip_project",
			"tip_repo_selector",
			"group_staged",
			"group_unstaged",
			"tip_group_by_dir",
			"group_conflicted",
			"src_staged_diff",
			"src_unstaged_diff",
			"btn_copy_cancel",
			"status_copy_cancelled",
			"paste_loading",
			"paste_loading_refused",
			"src_commit_short",
			"workspace_close",
			"workspace_open",
			"workspace_open_folder",
			"workspace_recent",
			"workspace_closed",
			"workspace_not_open",
			"workspace_none",
			"workspace_busy_applying",
			"workspace_draining",
			"workspace_drain_timeout",
			"workspace_drain_leaked",
			"remote_section",
			"remote_no_hosts",
			"remote_path_placeholder",
			"remote_open_path",
			"remote_opening",
			"remote_path_missing",
			"remote_host_missing",
			"remote_loading",
			"remote_recent",
			"remote_opened",
			"remote_open_failed",
			"remote_unsupported",
			"changes_scanning",
			"changes_loading",
			"changes_no_repository",
			"changes_no_match",
			"changes_clean_partial",
			"log_loading",
			"log_no_repository",
			"log_failed_feeds",
			"remote_worker_too_old",
			"remote_scan_failed",
			"remote_scan_incomplete",
			"remote_refs_too_large",
			"lifecycle_jobs",
			"tip_workspace_menu",
			"status_repo_vanished",
			"change_not_utf8",
			"tree_name_not_utf8",
			"diff_fold",
			"tip_diff_side",
			"tip_diff_unified",
			"tip_next_diff",
			"tip_prev_diff",
			"tip_match_case",
			"tip_regex",
			"tip_find_close",
			"tip_goto",
			"tip_close_tab",
			"tip_preview_tab",
			"tip_expand_folds",
			"diff_fold_end",
			"status_fold_failed",
			"status_fold_stale",
			"status_fold_too_large",
			"log_chip_branch",
			"log_paths_placeholder",
			"log_branch_placeholder",
			"log_loading_more",
			"log_dir_files",
			"log_details_in_branches",
			"log_details_show_all",
			"log_today",
			"log_yesterday",
			"changes_repo_error",
			"changes_unreadable",
			"changes_truncated",
			"log_chip_repo",
			"log_repo_all",
			"log_details_repo",
			"status_log_cross_repo",
			"status_log_tree_other_repo",
			"status_log_merged_cap",
			"log_selection_header",
			"status_selection_truncated",
			"menu_copy_files",
			"status_copied_limit",
			"status_copy_nothing_skipped",
			"paste_err_plan",
			"paste_err_destination",
			"paste_err_destination_not_dir",
			"paste_commit_count",
			"commit_no_message",
			"commit_empty_note",
			"commit_header_counts",
			"reason_skip_generic",
			"op_overwrite_pending",
			"op_refused",
			"reason_commit_excluded",
			"reason_commit_overwrite_pending",
			"reason_refused_renamed_from_dir",
			"reason_refused_delete_dir",
			"reason_refused_dir_in_way",
			"reason_refused_file_in_way",
			"reason_refusal_cause_renamed_from_dir",
			"reason_refusal_cause_delete_dir",
			"reason_refusal_cause_dir_in_way",
			"reason_refusal_cause_file_in_way",
			"commit_header_refused_suffix",
			"paste_commit_count_refused",
			"commit_will_be_refused",
			"commit_replay_refused",
			"commit_replay_partial",
			"commit_replay_partial_refused",
			"reason_nc_binary",
			"reason_nc_non_utf8",
			"reason_nc_non_utf8_path",
			"reason_nc_unsupported",
			"reason_nc_unreadable",
			"reason_skip_unsafe_path",
			"reason_skip_non_utf8_target",
			"status_commits_copied",
			"status_commits_copied_skipped",
		];
		for key in test_keys {
			assert!(
				!t(key, Locale::ZhTw).is_empty(),
				"Missing ZhTw key: {key}"
			);
			assert!(!t(key, Locale::En).is_empty(), "Missing En key: {key}");
		}
	}

	#[test]
	fn msg_render_does_not_translate_raw_args_matching_keys_unless_key_arg_set()
	{
		// A commit subject or raw error matching an i18n key must NOT be translated
		let raw_msg = Msg::new(
			"commit_replay_refused",
			[
				"1".to_string(),
				"op_skip".to_string(),
				"op_skip".to_string(),
			],
		);
		let rendered = raw_msg.render(Locale::ZhTw);
		assert!(rendered.contains("「op_skip」"), "{rendered}");
		assert!(rendered.ends_with("：op_skip"), "{rendered}");
		assert!(!rendered.contains("跳過"), "{rendered}");

		// When with_key_arg is explicitly used, only that arg index is translated
		let key_msg = Msg::with_key_arg(
			"commit_replay_refused",
			[
				"1".to_string(),
				"op_skip".to_string(),
				"reason_refusal_cause_file_in_way".to_string(),
			],
			2,
		);
		let rendered = key_msg.render(Locale::ZhTw);
		assert!(rendered.contains("「op_skip」"), "{rendered}");
		assert!(rendered.ends_with("：父目錄被檔案佔住"), "{rendered}");

		// When with_key_args is explicitly used, multiple arg indices are translated
		let multi_key_msg = Msg::with_key_args(
			"commit_replay_refused",
			[
				"1".to_string(),
				"commit_no_message".to_string(),
				"reason_refusal_cause_file_in_way".to_string(),
			],
			[1, 2],
		);
		let rendered = multi_key_msg.render(Locale::ZhTw);
		assert!(rendered.contains("「（無訊息）」"), "{rendered}");
		assert!(rendered.ends_with("：父目錄被檔案佔住"), "{rendered}");
		let rendered_en = multi_key_msg.render(Locale::En);
		assert!(rendered_en.contains("\"(no message)\""), "{rendered_en}");
		assert!(
			rendered_en
				.ends_with(": a file is in the way of its parent directory"),
			"{rendered_en}"
		);

		// commit_replay_partial_refused translates the cause key at arg index 3
		let partial_msg = Msg::with_key_arg(
			"commit_replay_partial_refused",
			[
				"sha1".to_string(),
				"2".to_string(),
				"path.txt".to_string(),
				"reason_refusal_cause_file_in_way".to_string(),
			],
			3,
		);
		let rendered = partial_msg.render(Locale::ZhTw);
		assert!(
			rendered
				.contains("第 2 個 commit 被拒絕：path.txt：父目錄被檔案佔住"),
			"{rendered}"
		);
		let rendered_en = partial_msg.render(Locale::En);
		assert!(
			rendered_en.contains(
				"Commit #2 was refused: path.txt: a file is in the way of its parent directory"
			),
			"{rendered_en}"
		);
	}

	#[test]
	fn test_indexed_placeholder_formatting() {
		let msg =
			Msg::new("log_failed_feeds", ["2".to_string(), "a, b".to_string()]);
		assert_eq!(msg.render(Locale::ZhTw), "2 個儲存庫無法讀取：a, b");
		assert_eq!(
			msg.render(Locale::En),
			"2 repository(ies) could not be read: a, b"
		);
	}
}
