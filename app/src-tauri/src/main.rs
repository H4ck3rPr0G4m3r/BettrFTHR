//! BettrFTHR desktop app: library, settings, tray, and supervision of the capture engine.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod engine;
mod games;
mod library;
mod settings;
mod sound;

use engine::Engine;
use parking_lot::Mutex;
use serde_json::{Value, json};
use settings::{BufferMode, Settings};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Listener, Manager, State, WindowEvent};

struct Paths {
    settings: PathBuf,
    engine_config: PathBuf,
    thumbs: PathBuf,
}

struct AppState {
    paths: Paths,
    settings: Mutex<Settings>,
    engine: Engine,
    game: Mutex<Option<games::Game>>,
    detection_on: Arc<AtomicBool>,
}

impl AppState {
    fn wants_engine(&self) -> bool {
        let s = self.settings.lock();
        s.buffer_mode == BufferMode::Always || self.game.lock().is_some()
    }
}

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn write_engine_config(path: &Path, s: &Settings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&s.engine_config())?)?;
    Ok(())
}

fn allow_media(app: &AppHandle, s: &Settings, thumbs: &Path) {
    let _ = std::fs::create_dir_all(&s.out_dir);
    let scope = app.asset_protocol_scope();
    if let Err(e) = scope.allow_directory(&s.out_dir, true) {
        tracing::warn!("asset scope: {e}");
    }
    let _ = scope.allow_directory(thumbs, false);
}

// ---------------------------------------------------------------------------------------
// Commands

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn set_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Res<Settings> {
    let new = settings.sanitized();
    new.save(&state.paths.settings).map_err(err)?;
    let restart = {
        let mut cur = state.settings.lock();
        let restart = cur.engine_differs(&new);
        *cur = new.clone();
        restart
    };
    state.detection_on.store(new.game_detection, Ordering::Relaxed);
    allow_media(&app, &new, &state.paths.thumbs);
    if restart {
        write_engine_config(&state.paths.engine_config, &new).map_err(err)?;
        state.engine.restart();
    }
    state.engine.set_wanted(state.wants_engine());
    Ok(new)
}

#[tauri::command]
fn engine_state(state: State<AppState>) -> engine::EngineState {
    state.engine.state()
}

#[tauri::command]
fn engine_command(state: State<AppState>, cmd: Value) -> Res<()> {
    state.engine.send(cmd)
}

#[tauri::command]
fn restart_engine(state: State<AppState>) {
    state.engine.restart();
}

#[tauri::command]
fn current_game(state: State<AppState>) -> Option<games::Game> {
    state.game.lock().clone()
}

#[tauri::command]
async fn list_clips(state: State<'_, AppState>) -> Res<Vec<library::Clip>> {
    let root = state.settings.lock().out_dir.clone();
    let thumbs = state.paths.thumbs.clone();
    tauri::async_runtime::spawn_blocking(move || library::scan(&root, &thumbs))
        .await
        .map_err(err)
}

#[tauri::command]
fn store_thumb(state: State<AppState>, path: PathBuf, jpeg: Vec<u8>, duration: f64) -> Res<PathBuf> {
    library::store_thumb(&path, &state.paths.thumbs, &jpeg, duration).map_err(err)
}

#[tauri::command]
fn rename_clip(state: State<AppState>, path: PathBuf, title: String) -> Res<PathBuf> {
    let root = state.settings.lock().out_dir.clone();
    library::ensure_inside(&root, &path).map_err(err)?;
    library::rename(&path, &title).map_err(err)
}

#[tauri::command]
fn delete_clips(state: State<AppState>, paths: Vec<PathBuf>) -> Res<()> {
    let root = state.settings.lock().out_dir.clone();
    for p in &paths {
        library::ensure_inside(&root, p).map_err(err)?;
    }
    library::recycle(&paths).map_err(err)
}

/// Opens the clip folder in Explorer (done here: the JS opener API needs a path scope).
#[tauri::command]
fn open_clip_folder(app: AppHandle, state: State<AppState>) -> Res<()> {
    use tauri_plugin_opener::OpenerExt;
    let dir = state.settings.lock().out_dir.clone();
    std::fs::create_dir_all(&dir).map_err(err)?;
    app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(err)
}

#[tauri::command]
fn reveal_clip(app: AppHandle, state: State<AppState>, path: PathBuf) -> Res<()> {
    use tauri_plugin_opener::OpenerExt;
    let root = state.settings.lock().out_dir.clone();
    library::ensure_inside(&root, &path).map_err(err)?;
    app.opener().reveal_item_in_dir(&path).map_err(err)
}

#[tauri::command]
fn copy_clip(path: PathBuf) -> Res<()> {
    clipboard::copy_file(&path).map_err(err)
}

/// Audio endpoints for the source picker. Asks the engine binary directly so it works even
/// while the engine is idle.
#[tauri::command]
async fn list_audio_devices() -> Res<Vec<Value>> {
    let exe = engine::find_exe().ok_or("bettr-engine.exe not found")?;
    tauri::async_runtime::spawn_blocking(move || {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new(exe)
            .arg("--list-audio")
            .creation_flags(0x0800_0000)
            .output()
            .map_err(err)?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    })
    .await
    .map_err(err)?
}

// ---------------------------------------------------------------------------------------

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open BettrFTHR", true, None::<&str>)?;
    let save = MenuItem::with_id(app, "save", "Save clip", true, None::<&str>)?;
    let record = MenuItem::with_id(app, "record", "Start / stop recording", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &save, &record, &sep, &quit])?;
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().expect("window icon"))
        .tooltip("BettrFTHR")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| {
            let state = app.state::<AppState>();
            match ev.id.as_ref() {
                "show" => show_main(app),
                "save" => {
                    let _ = state.engine.send(json!({"cmd": "save"}));
                }
                "record" => {
                    let _ = state.engine.send(json!({"cmd": "record_toggle"}));
                }
                "quit" => {
                    state.engine.shutdown();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&config_dir)?;
            std::fs::create_dir_all(&data_dir)?;
            let paths = Paths {
                settings: config_dir.join("settings.json"),
                engine_config: config_dir.join("engine.json"),
                thumbs: data_dir.join("thumbs"),
            };
            let settings = Settings::load(&paths.settings);
            if !paths.settings.exists() {
                let _ = settings.save(&paths.settings);
            }
            write_engine_config(&paths.engine_config, &settings)?;
            allow_media(&handle, &settings, &paths.thumbs);

            let want = settings.buffer_mode == BufferMode::Always;
            let engine = Engine::start(
                handle.clone(),
                engine::find_exe(),
                paths.engine_config.clone(),
                data_dir.join("engine.log"),
                want,
            );
            let detection_on = Arc::new(AtomicBool::new(settings.game_detection));
            app.manage(AppState {
                paths,
                settings: Mutex::new(settings),
                engine,
                game: Mutex::new(None),
                detection_on: detection_on.clone(),
            });

            {
                let h = handle.clone();
                games::spawn_detector(
                    move || detection_on.load(Ordering::Relaxed),
                    move |game| {
                        let state = h.state::<AppState>();
                        *state.game.lock() = game.clone();
                        state.engine.set_game(game.as_ref().map(|g| g.name.clone()));
                        state.engine.set_wanted(state.wants_engine());
                        let _ = h.emit("game", game);
                    },
                );
            }

            // Audible confirmation, mostly for when the app is hidden behind a game.
            {
                let h = handle.clone();
                app.listen_any("engine-event", move |ev| {
                    let Ok(v) = serde_json::from_str::<Value>(ev.payload()) else {
                        return;
                    };
                    let kind = v.get("event").and_then(|e| e.as_str()).unwrap_or_default();
                    let on = h.state::<AppState>().settings.lock().save_sound;
                    match kind {
                        "clip_saved" if on => sound::play(sound::Cue::Saved),
                        "recording_started" if on => sound::play(sound::Cue::RecordStart),
                        "recording_stopped" if on => sound::play(sound::Cue::RecordStop),
                        _ => {}
                    }
                });
            }

            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                let app = window.app_handle();
                let state = app.state::<AppState>();
                if state.settings.lock().close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    state.engine.shutdown();
                    app.exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            engine_state,
            engine_command,
            restart_engine,
            current_game,
            list_clips,
            store_thumb,
            rename_clip,
            delete_clips,
            open_clip_folder,
            reveal_clip,
            copy_clip,
            list_audio_devices,
        ])
        .build(tauri::generate_context!())
        .expect("build tauri app")
        .run(|app, ev| {
            if let tauri::RunEvent::ExitRequested { .. } = ev {
                app.state::<AppState>().engine.shutdown();
            }
        });
}
