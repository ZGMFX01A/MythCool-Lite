//! Myth.Cool Lite Tauri 2 应用程序主入口

pub mod autostart;
pub mod commands;
pub mod device;
pub mod shutdown;
pub mod streamer;

use commands::AppState;
use std::sync::{atomic::Ordering, Arc, Mutex};
use std::time::Duration;
use streamer::StreamManager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WebviewWindowBuilder};

fn create_main_window(app: &tauri::AppHandle) -> Result<(), String> {
    if app.get_webview_window("main").is_some() {
        return Ok(());
    }

    let window_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(|| "找不到 main 窗口配置".to_string())?;

    WebviewWindowBuilder::from_config(app, window_config)
        .map_err(|e| format!("创建管理界面失败: {}", e))?
        .build()
        .map_err(|e| format!("创建管理界面失败: {}", e))?;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if app.get_webview_window("main").is_none() {
        if let Err(e) = create_main_window(app) {
            eprintln!("{}", e);
            return;
        }
    }

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn start_background_stream(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let Some(saved_config) = commands::load_saved_stream_config(&state) else {
        eprintln!("未找到已保存的推流配置，静默模式等待管理界面配置");
        return;
    };

    // 设备可能在开机自启时尚未枚举完成；有限频率重试，避免重新引入 WebView 轮询。
    let manager = Arc::clone(&state.stream_manager);
    let cancel = Arc::clone(&state.background_start_cancel);
    cancel.store(false, Ordering::SeqCst);
    let config = saved_config;

    std::thread::spawn(move || {
        let mut config = config;
        loop {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }

            if !std::path::Path::new(&config.file_path).exists() {
                eprintln!("静默推流媒体文件不存在: {}", config.file_path);
                return;
            }

            let mut mgr = manager.lock().unwrap();
            let (spec, presence) = mgr.get_device_specs();
            if presence.ms_online || presence.vk_online {
                config.target_width = Some(spec.glass_w);
                config.target_height = Some(spec.glass_h);
                match mgr.start(config.clone()) {
                    Ok(()) => return,
                    Err(e) => {
                        eprintln!("静默推流启动失败: {}", e);
                    }
                }
            }
            drop(mgr);
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let start_minimized = args.iter().any(|a| a == "--minimized");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            stream_manager: Arc::new(Mutex::new(StreamManager::new())),
            start_minimized,
            stream_config_path: Mutex::new(None),
            background_start_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
        .invoke_handler(tauri::generate_handler![
            commands::check_device,
            commands::get_stream_status,
            commands::start_stream,
            commands::stop_stream,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_start_minimized,
            commands::get_saved_stream_config,
            commands::inspect_media,
            commands::get_device_specs,
        ])
        .setup(move |app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|e| format!("获取应用配置目录失败: {}", e))?;
            std::fs::create_dir_all(&config_dir)
                .map_err(|e| format!("创建应用配置目录失败: {}", e))?;
            let state = app.state::<AppState>();
            commands::set_stream_config_path(&state, config_dir.join("stream-config.json"));

            // 初始化 Windows 关机与错误抑制监听
            shutdown::init_windows_shutdown_handler(Arc::clone(&state.stream_manager));

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
                        show_main_window(app);
                    }
                    "stop" => {
                        let state: tauri::State<AppState> = app.state();
                        state.background_start_cancel.store(true, Ordering::SeqCst);
                        let mut mgr = state.stream_manager.lock().unwrap();
                        mgr.stop();
                    }
                    "quit" => {
                        let state: tauri::State<AppState> = app.state();
                        state.background_start_cancel.store(true, Ordering::SeqCst);
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
                        show_main_window(&app);
                    }
                })
                .build(app);

            if let Err(e) = tray_res {
                eprintln!("初始化托盘图标失败 (可忽略): {}", e);
            }

            if start_minimized {
                if autostart::is_autostart_enabled().unwrap_or(false) {
                    start_background_stream(app.handle());
                }
            } else {
                create_main_window(app.handle())?;
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭管理界面时销毁 WebView2，但保留托盘和后台推流。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.destroy();
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Myth.Cool Lite Tauri 应用程序发生错误")
        .run(|app, event| {
            match event {
                // 销毁最后一个 WebView 窗口时，阻止 Tauri 因“无窗口”结束应用。
                // 若系统正在关机（shutdown::is_shutting_down()）或托盘菜单显式调用 app.exit(0)，则正常放行退出。
                tauri::RunEvent::ExitRequested { api, code, .. } => {
                    if code.is_none() && !shutdown::is_shutting_down() {
                        api.prevent_exit();
                    }
                }
                tauri::RunEvent::Exit => {
                    let state: tauri::State<AppState> = app.state();
                    state.background_start_cancel.store(true, Ordering::SeqCst);
                    let mut mgr = state.stream_manager.lock().unwrap();
                    mgr.stop();
                }
                _ => {}
            }
        });
}
