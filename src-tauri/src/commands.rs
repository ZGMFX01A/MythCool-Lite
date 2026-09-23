//! 前端与 Tauri 后端交互的 Commands
//! 包括设备检测、推流控制、自启动切换、媒体尺寸检测与缩略帧提取。

use crate::autostart;
use crate::device;
use crate::streamer::{resolve_ffmpeg, StreamConfig, StreamManager, StreamStatus};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use tauri::State;

pub struct AppState {
    pub stream_manager: Arc<Mutex<StreamManager>>,
    pub start_minimized: bool,
    pub stream_config_path: Mutex<Option<PathBuf>>,
    pub background_start_cancel: Arc<AtomicBool>,
}

#[derive(serde::Serialize)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub preview_base64: String,
}

#[derive(serde::Serialize)]
pub struct DeviceSpecs {
    pub online: bool,
    pub width: u32,
    pub height: u32,
    pub stream_width: u32,
    pub stream_height: u32,
    pub aspect_ratio_str: String,
    pub active_kind: Option<String>,
    pub ms_online: bool,
    pub vk_online: bool,
    pub max_fps: u32,
}

#[derive(serde::Serialize)]
pub struct CheckDeviceResult {
    pub ready: bool,
    pub active_kind: Option<String>,
    pub ms_online: bool,
    pub vk_online: bool,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

pub fn check_device_with_opener<F, D>(opener: F) -> CheckDeviceResult
where
    F: FnOnce() -> Result<(D, device::DevicePresence), device::ProbeError>,
    D: device::ScreenDevice,
{
    match opener() {
        Ok((mut dev, presence)) => {
            let active_kind = format!("{:?}", dev.spec().kind);
            // 契约：仅完成无副作用的打开与探测，不调用 arm，随即立即释放
            dev.close();
            CheckDeviceResult {
                ready: true,
                active_kind: Some(active_kind),
                ms_online: presence.ms_online,
                vk_online: presence.vk_online,
                error_code: None,
                error_message: None,
            }
        }
        Err(e) => {
            let (code, msg) = match &e {
                device::ProbeError::AccessDenied(c) => (Some(*c), e.to_string()),
                device::ProbeError::DriverMissing(_) => (Some(-2), e.to_string()),
                device::ProbeError::NotFound => (Some(-1), e.to_string()),
                device::ProbeError::Io(_) => (Some(-3), e.to_string()),
            };
            let presence = device::probe_all(None).map(|p| p.presence).unwrap_or_default();
            CheckDeviceResult {
                ready: false,
                active_kind: None,
                ms_online: presence.ms_online,
                vk_online: presence.vk_online,
                error_code: code,
                error_message: Some(msg),
            }
        }
    }
}

#[tauri::command]
pub fn check_device() -> CheckDeviceResult {
    check_device_with_opener(|| device::open_active_device(Arc::new(device::Noop)))
}

#[tauri::command]
pub fn get_device_specs(state: State<'_, AppState>) -> DeviceSpecs {
    let mgr = state.stream_manager.lock().unwrap();
    let (spec, presence) = mgr.get_device_specs();

    let gcd = num_gcd(spec.glass_w, spec.glass_h);
    let aspect_ratio_str = format!("{}:{}", spec.glass_w / gcd, spec.glass_h / gcd);

    DeviceSpecs {
        online: presence.ms_online || presence.vk_online,
        width: spec.glass_w,
        height: spec.glass_h,
        stream_width: spec.stream_w,
        stream_height: spec.stream_h,
        aspect_ratio_str,
        active_kind: Some(format!("{:?}", spec.kind)),
        ms_online: presence.ms_online,
        vk_online: presence.vk_online,
        max_fps: spec.max_fps,
    }
}

fn num_gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    if a == 0 {
        1
    } else {
        a
    }
}

#[tauri::command]
pub fn get_stream_status(state: State<'_, AppState>) -> StreamStatus {
    let mgr = state.stream_manager.lock().unwrap();
    mgr.get_status()
}

#[tauri::command]
pub fn start_stream(config: StreamConfig, state: State<'_, AppState>) -> Result<(), String> {
    state
        .background_start_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut mgr = state.stream_manager.lock().unwrap();
    mgr.start(config.clone())?;
    drop(mgr);

    // 后台静默启动不依赖 WebView 的 localStorage，保存最近一次成功启动的配置。
    if let Err(e) = save_stream_config(&state, &config) {
        eprintln!("保存后台推流配置失败 (不影响本次推流): {}", e);
    }
    Ok(())
}

#[tauri::command]
pub fn stop_stream(state: State<'_, AppState>) -> Result<(), String> {
    state
        .background_start_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut mgr = state.stream_manager.lock().unwrap();
    mgr.stop();
    Ok(())
}

#[tauri::command]
pub fn get_autostart() -> Result<bool, String> {
    autostart::is_autostart_enabled()
}

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    autostart::set_autostart(enabled)
}

#[tauri::command]
pub fn get_start_minimized(state: State<'_, AppState>) -> bool {
    state.start_minimized
}

/// 返回最近一次成功启动的推流配置，供静默启动和 GUI 恢复使用。
#[tauri::command]
pub fn get_saved_stream_config(state: State<'_, AppState>) -> Option<StreamConfig> {
    load_stream_config(&state).ok().flatten()
}

pub fn set_stream_config_path(state: &AppState, path: PathBuf) {
    *state.stream_config_path.lock().unwrap() = Some(path);
}

pub fn load_saved_stream_config(state: &AppState) -> Option<StreamConfig> {
    load_stream_config(state).ok().flatten()
}

fn stream_config_path(state: &AppState) -> Result<PathBuf, String> {
    state
        .stream_config_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "推流配置路径尚未初始化".to_string())
}

fn save_stream_config(state: &AppState, config: &StreamConfig) -> Result<(), String> {
    let path = stream_config_path(state)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let data =
        serde_json::to_vec_pretty(config).map_err(|e| format!("序列化推流配置失败: {}", e))?;
    std::fs::write(path, data).map_err(|e| format!("写入推流配置失败: {}", e))
}

fn load_stream_config(state: &AppState) -> Result<Option<StreamConfig>, String> {
    let path = stream_config_path(state)?;
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取推流配置失败: {}", e)),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|e| format!("解析推流配置失败: {}", e))
}

/// 提取媒体文件的尺寸和首帧 Base64 预览图
#[tauri::command]
pub fn inspect_media(file_path: String) -> Result<MediaInfo, String> {
    let ffmpeg = resolve_ffmpeg(None)?;
    let mut cmd = Command::new(&ffmpeg);
    cmd.arg("-ss")
        .arg("00:00:00.100")
        .arg("-i")
        .arg(&file_path)
        .arg("-vf")
        .arg("scale=960:960:force_original_aspect_ratio=decrease")
        .arg("-vframes")
        .arg("1")
        .arg("-f")
        .arg("image2pipe")
        .arg("-vcodec")
        .arg("mjpeg")
        .arg("pipe:1");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let output = cmd
        .output()
        .map_err(|e| format!("执行 ffmpeg 提取预览失败: {}", e))?;
    if !output.status.success() || output.stdout.is_empty() {
        // 如果 0.1s 失败，尝试 0s
        let mut cmd2 = Command::new(&ffmpeg);
        cmd2.arg("-i")
            .arg(&file_path)
            .arg("-vf")
            .arg("scale=960:960:force_original_aspect_ratio=decrease")
            .arg("-vframes")
            .arg("1")
            .arg("-f")
            .arg("image2pipe")
            .arg("-vcodec")
            .arg("mjpeg")
            .arg("pipe:1");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd2.creation_flags(0x08000000);
        }
        let out2 = cmd2
            .output()
            .map_err(|e| format!("执行 ffmpeg 提取预览失败: {}", e))?;
        if !out2.status.success() || out2.stdout.is_empty() {
            return Err("无法从媒体文件中提取有效画面帧".to_string());
        }
        return Ok(finish_preview_info(&ffmpeg, &file_path, out2.stdout));
    }

    Ok(finish_preview_info(&ffmpeg, &file_path, output.stdout))
}

fn finish_preview_info(
    ffmpeg: &std::path::Path,
    file_path: &str,
    jpeg_bytes: Vec<u8>,
) -> MediaInfo {
    let b64 = format!(
        "data:image/jpeg;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpeg_bytes)
    );

    // 探测真实分辨率
    let (w, h) = probe_resolution(ffmpeg, file_path).unwrap_or((640, 480));

    MediaInfo {
        width: w,
        height: h,
        preview_base64: b64,
    }
}

/// 用 ffprobe 或 ffmpeg 探测视频/图片分辨率
fn probe_resolution(ffmpeg_path: &std::path::Path, file_path: &str) -> Option<(u32, u32)> {
    let ffprobe_path = ffmpeg_path.with_file_name("ffprobe.exe");
    if ffprobe_path.exists() {
        let mut cmd = Command::new(ffprobe_path);
        cmd.arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg("stream=width,height")
            .arg("-of")
            .arg("csv=s=x:p=0")
            .arg(file_path);

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }

        if let Ok(out) = cmd.output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                let trimmed = s.trim();
                let parts: Vec<&str> = trimmed.split('x').collect();
                if parts.len() == 2 {
                    if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                        return Some((w, h));
                    }
                }
            }
        }
    }
    None
}
