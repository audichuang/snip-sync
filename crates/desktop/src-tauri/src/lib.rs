use log::{info, warn};
use tauri::menu::MenuBuilder;
use tauri::tray::{
	MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_positioner::{Position, WindowExt};

mod commands;

use commands::AppState;

/// Webview event: the tray asked for a paste preview.
const EVENT_PASTE: &str = "tray-paste";
/// Webview event: a tray copy finished (`CopyDone`) or failed (string).
const EVENT_COPIED: &str = "tray-copied";
const EVENT_COPY_FAILED: &str = "tray-copy-failed";

const PASTE_ID: &str = "paste";
const COPY_LAST_ID: &str = "copy-last";
const SHOW_ID: &str = "show";
const QUIT_ID: &str = "quit";

fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
	app.get_webview_window("main")
}

fn focus(window: &WebviewWindow) {
	let _ = window.show();
	let _ = window.unminimize();
	let _ = window.set_focus();
}

fn show_main(app: &AppHandle) {
	if let Some(window) = main_window(app) {
		focus(&window);
	}
}

fn on_menu(app: &AppHandle, id: &str) {
	match id {
		PASTE_ID => {
			show_main(app);
			let _ = app.emit(EVENT_PASTE, ());
		}
		COPY_LAST_ID => {
			let app = app.clone();
			tauri::async_runtime::spawn_blocking(move || {
				let state = app.state::<AppState>();
				match commands::repeat_last_copy(&app, &state) {
					Ok(done) => {
						let _ = app.emit(EVENT_COPIED, done);
					}
					Err(e) => {
						warn!("tray copy failed: {e}");
						let _ = app.emit(EVENT_COPY_FAILED, e);
					}
				}
			});
		}
		SHOW_ID => show_main(app),
		QUIT_ID => app.exit(0),
		_ => {}
	}
}

fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
	let menu = MenuBuilder::new(app)
		.text(PASTE_ID, "Paste from Clipboard")
		.text(COPY_LAST_ID, "Copy Last Selection")
		.separator()
		.text(SHOW_ID, "Open Main Window")
		.text(QUIT_ID, "Quit snip-sync")
		.build()?;

	let mut tray = TrayIconBuilder::with_id("main")
		.menu(&menu)
		.show_menu_on_left_click(false)
		.tooltip("snip-sync")
		.on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
		.on_tray_icon_event(|tray, event| {
			tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
			if let TrayIconEvent::Click {
				button: MouseButton::Left,
				button_state: MouseButtonState::Up,
				..
			} = event
			{
				// Left click opens the window next to the tray icon.
				if let Some(window) = main_window(tray.app_handle()) {
					let _ = window.move_window(Position::TrayCenter);
					focus(&window);
				}
			}
		});
	if let Some(icon) = app.default_window_icon().cloned() {
		tray = tray.icon(icon);
	}
	tray.build(app)?;
	Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
	tauri::Builder::default()
		// Must be first so a second launch only focuses the running one.
		.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
			show_main(app);
		}))
		.plugin(
			tauri_plugin_log::Builder::new()
				.level(log::LevelFilter::Info)
				.build(),
		)
		.plugin(tauri_plugin_store::Builder::default().build())
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_opener::init())
		.plugin(tauri_plugin_positioner::init())
		.plugin(
			tauri_plugin_autostart::Builder::new()
				.arg("--minimized")
				.build(),
		)
		.manage(AppState::default())
		.on_window_event(|window, event| {
			// Closing the main window only hides it; the tray keeps running.
			if let tauri::WindowEvent::CloseRequested { api, .. } = event {
				if window.label() == "main" {
					api.prevent_close();
					let _ = window.hide();
				}
			}
		})
		.setup(|app| {
			setup_tray(app)?;
			if std::env::args().any(|a| a == "--minimized") {
				if let Some(window) = main_window(app.handle()) {
					let _ = window.hide();
				}
			}
			info!("snip-sync desktop setup completed");
			Ok(())
		})
		.invoke_handler(tauri::generate_handler![
			commands::copy,
			commands::list_commits,
			commands::copy_commits,
			commands::read_clipboard_plan,
			commands::apply_restore,
			commands::replay_commits,
			commands::diff,
		])
		.run(tauri::generate_context!())
		.expect("error while running tauri application");
}
