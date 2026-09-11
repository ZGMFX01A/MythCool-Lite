//! Myth.Cool Lite Tauri 2 应用程序主入口

mod autostart;
mod commands;
mod device;
mod streamer;

use commands::AppState;
use std::sync::Mutex;
use streamer::StreamManager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let start_minimized = args.iter().any(|a| a == "--minimized");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            stream_manager: Mutex::new(StreamManager::new()),
            start_minimized,
        })
        .invoke_handler(tauri::generate_handler![
            commands::check_device,
            commands::get_stream_status,
            commands::start_stream,
            commands::stop_stream,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_start_minimized,
            commands::inspect_media,
            commands::get_device_specs,
        ])
        .setup(move |app| {
            // 系统托盘菜单
            let show_i = MenuItem::with_id(app, "show", "显示控制台", true, None::<&str>)?;
            let stop_i = MenuItem::with_id(app, "stop", "停止推流", true, None::<&str>)?;
            let sep_i = MenuItem::with_id(app, "sep", "---", false, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "退出程序", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &stop_i, &sep_i, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::new()
                .tooltip("Myth.Cool Lite 水冷副屏控制器")
                .menu(&menu)
                .show_menu_on_left_click(false);

            let icon = app
                .default_window_icon()
                .cloned()
                .or_else(|| Some(tauri::include_image!("icons/32x32.png")));

            if let Some(icon) = icon {
                tray_builder = tray_builder.icon(icon);
            }

            let tray_res = tray_builder
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                    "stop" => {
                        let state: tauri::State<AppState> = app.state();
                        let mut mgr = state.stream_manager.lock().unwrap();
                        mgr.stop();
                    }
                    "quit" => {
                        let state: tauri::State<AppState> = app.state();
                        let mut mgr = state.stream_manager.lock().unwrap();
                        mgr.stop();
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app);

            if let Err(e) = tray_res {
                eprintln!("初始化托盘图标失败 (可忽略): {}", e);
            }

            // 开机自启动静默模式：隐藏窗口
            if start_minimized {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 点击关闭按钮时最小化到系统托盘，防止断流黑屏
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Myth.Cool Lite Tauri 应用程序发生错误")
        .run(|app, event| {
            if matches!(
                event,
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
            ) {
                let state: tauri::State<AppState> = app.state();
                let mut mgr = state.stream_manager.lock().unwrap();
                mgr.stop();
            }
        });
}
