//! 后台推流控制器与 FFmpeg 管道管理
//! 负责协调 ScreenDevice 后端写入、FFmpeg 实时裁切缩放转码、静态图单帧驻留与统计数据同步。

use crate::device::{
    open_active_device, probe_all, DevicePresence, Noop, ScreenDevice, ScreenSpec,
};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamConfig {
    pub file_path: String,
    pub fps: u32,
    pub is_loop: bool,
    pub crop_x: Option<u32>,
    pub crop_y: Option<u32>,
    pub crop_w: Option<u32>,
    pub crop_h: Option<u32>,
    pub scale_mode: String, // "custom", "cover", "contain", "stretch"
    /// 仅 VK 屏允许用户覆盖，MS 屏强制忽略以防破坏协议与画面方向
    pub target_width: Option<u32>,
    pub target_height: Option<u32>,
    pub custom_ffmpeg: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamStatus {
    pub is_running: bool,
    pub device_online: bool,
    pub active_kind: Option<String>,
    pub ms_online: bool,
    pub vk_online: bool,
    pub target_width: u32,
    pub target_height: u32,
    pub stream_width: u32,
    pub stream_height: u32,
    pub current_file: String,
    pub pushed_frames: u64,
    pub current_fps: f32,
    pub requested_fps: u32,
    pub effective_fps: u32,
    pub error_message: Option<String>,
    pub last_error: Option<String>,
}

impl Default for StreamStatus {
    fn default() -> Self {
        Self {
            is_running: false,
            device_online: false,
            active_kind: None,
            ms_online: false,
            vk_online: false,
            target_width: 640,
            target_height: 480,
            stream_width: 640,
            stream_height: 480,
            current_file: String::new(),
            pushed_frames: 0,
            current_fps: 0.0,
            requested_fps: 16,
            effective_fps: 16,
            error_message: None,
            last_error: None,
        }
    }
}

pub struct StreamManager {
    status: Arc<Mutex<StreamStatus>>,
    stop_signal: Arc<AtomicBool>,
    worker_handle: Option<thread::JoinHandle<()>>,
    worker_done: Arc<AtomicBool>,
    cached_spec: Arc<Mutex<ScreenSpec>>,
    cached_presence: Arc<Mutex<DevicePresence>>,
    child_handle: Arc<Mutex<Option<Child>>>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StreamStatus::default())),
            stop_signal: Arc::new(AtomicBool::new(false)),
            worker_handle: None,
            worker_done: Arc::new(AtomicBool::new(true)),
            cached_spec: Arc::new(Mutex::new(ScreenSpec::VK_DEFAULT)),
            cached_presence: Arc::new(Mutex::new(DevicePresence::default())),
            child_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 获取当前推流运行状态
    pub fn get_status(&self) -> StreamStatus {
        let mut s = self.status.lock().unwrap().clone();
        if s.is_running {
            let presence = *self.cached_presence.lock().unwrap();
            s.device_online = true;
            s.ms_online = presence.ms_online;
            s.vk_online = presence.vk_online;
        }
        let spec = *self.cached_spec.lock().unwrap();
        s.target_width = spec.glass_w;
        s.target_height = spec.glass_h;
        s.stream_width = spec.stream_w;
        s.stream_height = spec.stream_h;
        s
    }

    /// 探测或获取设备的实际规格与双屏在线状态
    pub fn get_device_specs(&self) -> (ScreenSpec, DevicePresence) {
        let is_running = self.status.lock().unwrap().is_running;
        if is_running {
            let spec = *self.cached_spec.lock().unwrap();
            let presence = *self.cached_presence.lock().unwrap();
            return (spec, presence);
        }

        match probe_all(None) {
            Ok(probe) => {
                let spec = probe.selected.spec;
                let presence = probe.presence;
                *self.cached_spec.lock().unwrap() = spec;
                *self.cached_presence.lock().unwrap() = presence;

                let mut st = self.status.lock().unwrap();
                st.device_online = true;
                st.active_kind = Some(format!("{:?}", spec.kind));
                st.ms_online = presence.ms_online;
                st.vk_online = presence.vk_online;
                st.target_width = spec.glass_w;
                st.target_height = spec.glass_h;
                st.stream_width = spec.stream_w;
                st.stream_height = spec.stream_h;

                (spec, presence)
            }
            Err(_) => {
                let mut st = self.status.lock().unwrap();
                st.device_online = false;
                st.ms_online = false;
                st.vk_online = false;
                st.active_kind = None;

                let spec = *self.cached_spec.lock().unwrap();
                let presence = DevicePresence::default();
                *self.cached_presence.lock().unwrap() = presence;
                (spec, presence)
            }
        }
    }

    /// 停止当前正在运行的推流
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(child) = self.child_handle.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        if let Some(handle) = self.worker_handle.take() {
            // 使用带超时的非阻塞等待（1.5 秒），避免底层 USB 或驱动 I/O 假死导致主线程无条件挂起
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            if rx.recv_timeout(Duration::from_millis(1500)).is_err() {
                eprintln!("[WARN] 推流工作线程未能在 1.5 秒内响应停止退出，转入后台安全清理以保证 UI 响应");
            }
        }
        self.child_handle.lock().unwrap().take();
        let mut st = self.status.lock().unwrap();
        st.is_running = false;
        st.current_fps = 0.0;
        st.error_message = None;
    }

    /// 启动推流任务
    ///
    /// 【契约】arm() 必须位于此处执行，从而统一覆盖 commands 手动启动和后台静默自启动。
    pub fn start(&mut self, config: StreamConfig) -> Result<(), String> {
        // 先停掉旧任务
        self.stop();

        // 如果底层 USB I/O 仍未返回，旧 worker 可能还持有设备句柄。
        // 此时拒绝并发启动，避免两个 worker 交叉操作同一块屏幕或互相杀掉 FFmpeg。
        if !self.worker_done.load(Ordering::SeqCst) {
            return Err("上一条推流任务仍在退出，请稍后再试".to_string());
        }

        // 查找 ffmpeg 可执行文件路径
        let ffmpeg_path = resolve_ffmpeg(config.custom_ffmpeg.as_deref())?;

        // 检查媒体文件是否存在
        let media_path = Path::new(&config.file_path);
        if !media_path.exists() {
            return Err(format!("媒体文件不存在: {}", config.file_path));
        }

        // 打开活跃屏幕设备（无副作用探测级打开）
        let (mut device, presence) = open_active_device(Arc::new(Noop))
            .map_err(|e| format!("打开屏幕设备失败: {}", e))?;

        // 显式 arm 设备（MS 关闭屏闸/视频等待推流，VK 初始化双轮唤醒与几何校验）
        if let Err(e) = device.arm() {
            device.abort_arm();
            return Err(format!("初始化/arm 屏幕失败: {}", e));
        }

        let spec = *device.spec();

        // 硬件推流尺寸与设备规格强绑定：若传入覆盖尺寸不等于硬件规格，输出提示并保持硬件规格，
        // 确保 FFmpeg 产出的帧大小与后端推流校验尺寸绝对一致，杜绝帧长度不匹配导致的推流崩溃。
        if let (Some(w), Some(h)) = (config.target_width, config.target_height) {
            if w > 0 && h > 0 && (w != spec.stream_w || h != spec.stream_h) {
                eprintln!(
                    "[WARN] 请求的推流尺寸 {}x{} 与硬件设备规格 {}x{} 不一致，忽略不支持的覆盖值并采用设备硬件规格",
                    w, h, spec.stream_w, spec.stream_h
                );
            }
        }

        let effective_fps = config.fps.max(5).min(spec.max_fps);

        self.stop_signal.store(false, Ordering::SeqCst);
        let stop_flag = Arc::clone(&self.stop_signal);
        let worker_done = Arc::new(AtomicBool::new(false));
        self.worker_done = Arc::clone(&worker_done);
        let status_arc = Arc::clone(&self.status);
        let cached_spec_arc = Arc::clone(&self.cached_spec);
        let cached_presence_arc = Arc::clone(&self.cached_presence);
        let child_handle = Arc::clone(&self.child_handle);

        *cached_spec_arc.lock().unwrap() = spec;
        *cached_presence_arc.lock().unwrap() = presence;

        {
            let mut st = status_arc.lock().unwrap();
            st.is_running = true;
            st.device_online = true;
            st.active_kind = Some(format!("{:?}", spec.kind));
            st.ms_online = presence.ms_online;
            st.vk_online = presence.vk_online;
            st.current_file = config.file_path.clone();
            st.pushed_frames = 0;
            st.current_fps = 0.0;
            st.requested_fps = config.fps;
            st.effective_fps = effective_fps;
            st.target_width = spec.glass_w;
            st.target_height = spec.glass_h;
            st.stream_width = spec.stream_w;
            st.stream_height = spec.stream_h;
            st.error_message = None;
            st.last_error = None;
        }

        let handle = thread::spawn(move || {
            run_stream_worker(
                device,
                spec,
                ffmpeg_path,
                config,
                effective_fps,
                stop_flag,
                status_arc,
                child_handle,
                worker_done,
            );
        });

        self.worker_handle = Some(handle);
        Ok(())
    }
}

/// 解析可用的 ffmpeg.exe 路径（支持绿色便携版就近探测）
pub fn resolve_ffmpeg(custom: Option<&str>) -> Result<PathBuf, String> {
    if let Some(c) = custom {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }

    // 1. 优先就近查找当前 EXE 同级或子目录
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let local_candidates = [
                dir.join("ffmpeg.exe"),
                dir.join("bin").join("ffmpeg").join("ffmpeg.exe"),
                dir.join("ffmpeg").join("ffmpeg.exe"),
                dir.join("..").join("bin").join("ffmpeg").join("ffmpeg.exe"),
                dir.join("..")
                    .join("..")
                    .join("bin")
                    .join("ffmpeg")
                    .join("ffmpeg.exe"),
            ];
            for c in &local_candidates {
                if c.exists() {
                    return Ok(c.clone());
                }
            }
        }
    }

    // 2. 备用候选路径
    let candidates = [
        PathBuf::from(r"bin\ffmpeg\ffmpeg.exe"),
        PathBuf::from(r"ffmpeg.exe"),
    ];

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }

    // 3. 系统环境变量探测
    if let Ok(p) = which_ffmpeg() {
        return Ok(p);
    }

    Err("未找到 ffmpeg.exe 解码器。请确保程序目录下包含 bin\\ffmpeg 或 ffmpeg.exe。".to_string())
}

fn which_ffmpeg() -> Result<PathBuf, ()> {
    let output = Command::new("where").arg("ffmpeg").output();
    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(first_line) = s.lines().next() {
                let p = PathBuf::from(first_line.trim());
                if p.exists() {
                    return Ok(p);
                }
            }
        }
    }
    Err(())
}

/// 构造 FFmpeg 视频滤镜字符串
///
/// 【契约】：
/// - 缩放与裁切按 glass 尺寸 (glass_w, glass_h) 进行；
/// - MS 屏 (rotate_cw == 90) 在输入端必须追加 `transpose=1`（顺时针预旋转 90°）；
/// - VK 屏 (rotate_cw == 0) 不追加任何 transpose；
/// - 严禁使用 `transpose=2`（逆时针）。
pub fn build_filter_string(config: &StreamConfig, spec: &ScreenSpec) -> String {
    let target_w = spec.glass_w;
    let target_h = spec.glass_h;
    let mut parts = Vec::new();

    if let (Some(w), Some(h), Some(x), Some(y)) =
        (config.crop_w, config.crop_h, config.crop_x, config.crop_y)
    {
        if w > 0 && h > 0 {
            parts.push(format!("crop={}:{}:{}:{}", w, h, x, y));
        }
    }

    match config.scale_mode.as_str() {
        "cover" => {
            parts.push(format!(
                "scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}",
                target_w, target_h, target_w, target_h
            ));
        }
        "contain" => {
            parts.push(format!(
                "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:black",
                target_w, target_h, target_w, target_h
            ));
        }
        "stretch" => {
            parts.push(format!(
                "scale={}:{}:flags=fast_bilinear",
                target_w, target_h
            ));
        }
        _ => {
            // custom 或自由模式：缩放到目标屏玻璃分辨率
            parts.push(format!(
                "scale={}:{}:flags=fast_bilinear",
                target_w, target_h
            ));
        }
    }

    if spec.rotate_cw == 90 {
        // MS 设备显示时硬件会自动逆时针旋转 90°，因此输入侧预先顺时针旋转 90°
        parts.push("transpose=1".to_string());
    }

    parts.join(",")
}

/// 启动 FFmpeg 子进程并返回管道
fn spawn_ffmpeg(
    ffmpeg_path: &Path,
    config: &StreamConfig,
    spec: &ScreenSpec,
    effective_fps: u32,
) -> Result<Child, String> {
    let filter = build_filter_string(config, spec);
    let fps_str = effective_fps.to_string();

    let mut cmd = Command::new(ffmpeg_path);
    cmd.arg("-v").arg("error");

    if config.is_loop {
        cmd.arg("-stream_loop").arg("-1");
    }

    cmd.arg("-re")
        .arg("-threads")
        .arg("2")
        .arg("-filter_threads")
        .arg("1")
        .arg("-i")
        .arg(&config.file_path)
        .arg("-vf")
        .arg(&filter)
        .arg("-r")
        .arg(&fps_str)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg(spec.src_pix_fmt) // MS 为 "bgr24", VK 为 "bgr565le"
        .arg("pipe:1");

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败: {}", e))
}

fn drain_ffmpeg_stderr(stderr: ChildStderr) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
}

fn install_ffmpeg_child(
    mut child: Child,
    child_handle: &Arc<Mutex<Option<Child>>>,
) -> Result<ChildStdout, String> {
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("无法获取 ffmpeg stdout".to_string());
        }
    };
    if let Some(stderr) = child.stderr.take() {
        drain_ffmpeg_stderr(stderr);
    }
    *child_handle.lock().unwrap() = Some(child);
    Ok(stdout)
}

fn kill_ffmpeg_child(child_handle: &Arc<Mutex<Option<Child>>>) {
    let mut slot = child_handle.lock().unwrap();
    if let Some(child) = slot.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    slot.take();
}

struct WorkerCompletion(Arc<AtomicBool>);

impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// 实际执行推流的工作线程
fn run_stream_worker(
    mut device: Box<dyn ScreenDevice>,
    spec: ScreenSpec,
    ffmpeg_path: PathBuf,
    config: StreamConfig,
    effective_fps: u32,
    stop_flag: Arc<AtomicBool>,
    status_arc: Arc<Mutex<StreamStatus>>,
    child_handle: Arc<Mutex<Option<Child>>>,
    worker_done: Arc<AtomicBool>,
) {
    let _completion = WorkerCompletion(worker_done);

    if stop_flag.load(Ordering::SeqCst) {
        device.close();
        return;
    }

    let frame_bytes = (spec.stream_w * spec.stream_h * spec.bytes_per_pixel) as usize;
    let mut buf = vec![0u8; frame_bytes];
    let mut last_frame = vec![0u8; frame_bytes];
    let mut has_frame = false;
    let mut pushed_count: u64 = 0;

    let frame_interval = Duration::from_millis(1000 / effective_fps as u64);

    let mut stdout = match spawn_ffmpeg(&ffmpeg_path, &config, &spec, effective_fps)
        .and_then(|child| install_ffmpeg_child(child, &child_handle))
    {
        Ok(stdout) => stdout,
        Err(e) => {
            device.abort_arm();
            device.close();
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            let mut st = status_arc.lock().unwrap();
            st.is_running = false;
            st.error_message = Some(e);
            return;
        }
    };

    let mut last_stat_time = Instant::now();
    let mut stat_frames = 0u64;

    while !stop_flag.load(Ordering::SeqCst) && !crate::shutdown::is_shutting_down() {
        let frame_start = Instant::now();
        let mut got = 0;
        let mut eof = false;

        // 从 stdout 读取完整的一帧 (stream_w * stream_h * bytes_per_pixel 字节)
        while got < frame_bytes {
            match stdout.read(&mut buf[got..frame_bytes]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => got += n,
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }

        if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
            break;
        }

        if !eof && got == frame_bytes {
            std::mem::swap(&mut buf, &mut last_frame);
            has_frame = true;
        } else {
            if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
                break;
            }

            if !has_frame {
                let mut st = status_arc.lock().unwrap();
                st.is_running = false;
                st.error_message = Some("未解码到有效画面（文件损坏或格式不支持）".to_string());
                kill_ffmpeg_child(&child_handle);
                device.abort_arm();
                device.close();
                return;
            }

            if config.is_loop {
                kill_ffmpeg_child(&child_handle);
                if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
                if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
                    break;
                }
                match spawn_ffmpeg(&ffmpeg_path, &config, &spec, effective_fps)
                    .and_then(|child| install_ffmpeg_child(child, &child_handle))
                {
                    Ok(new_stdout) => stdout = new_stdout,
                    Err(e) => {
                        if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
                            break;
                        }
                        let mut st = status_arc.lock().unwrap();
                        st.is_running = false;
                        st.error_message = Some(format!("循环重启 ffmpeg 失败: {}", e));
                        device.abort_arm();
                        device.close();
                        return;
                    }
                }
            }
        }

        // 推送最后一帧有效数据
        if has_frame {
            if let Err(e) = device.push_frame(&last_frame) {
                if stop_flag.load(Ordering::SeqCst) || crate::shutdown::is_shutting_down() {
                    kill_ffmpeg_child(&child_handle);
                    break;
                }
                let mut st = status_arc.lock().unwrap();
                st.is_running = false;
                st.last_error = Some(format!("推流中断: {}", e));
                st.error_message = Some(format!("推流中断: {}", e));
                kill_ffmpeg_child(&child_handle);
                if pushed_count == 0 {
                    device.abort_arm();
                }
                device.close();
                return;
            }
            pushed_count += 1;
            stat_frames += 1;
        }

        // 帧率控制
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }

        // 统计更新
        if last_stat_time.elapsed() >= Duration::from_secs(1) {
            let actual_fps = stat_frames as f32 / last_stat_time.elapsed().as_secs_f32();
            let mut st = status_arc.lock().unwrap();
            st.pushed_frames = pushed_count;
            st.current_fps = actual_fps;
            last_stat_time = Instant::now();
            stat_frames = 0;
        }
    }

    kill_ffmpeg_child(&child_handle);

    // 【重要契约】：此处刻意不发关屏/关视频指令，仅调用 device.close() 释放资源。
    // MS 与 VK 副屏面板自带显存/控制器，会保持推流的最后一帧静态画面，避免黑屏。
    device.close();

    let mut st = status_arc.lock().unwrap();
    st.is_running = false;
    st.current_fps = 0.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_completion_marks_exit_after_scope() {
        let done = Arc::new(AtomicBool::new(false));
        {
            let _completion = WorkerCompletion(Arc::clone(&done));
            assert!(!done.load(Ordering::SeqCst));
        }
        assert!(done.load(Ordering::SeqCst));
    }
}
