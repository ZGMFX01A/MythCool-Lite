//! 后台推流控制器与 FFmpeg 管道管理
//! 负责协调 WinUSB 设备写入、FFmpeg 实时裁切缩放转码、静态图单帧驻留与统计数据同步。

use crate::device::{find_device_path, DeviceGeometry, UsbDevice};
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
    pub target_width: Option<u32>,
    pub target_height: Option<u32>,
    pub custom_ffmpeg: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamStatus {
    pub is_running: bool,
    pub device_online: bool,
    pub target_width: u32,
    pub target_height: u32,
    pub current_file: String,
    pub pushed_frames: u64,
    pub current_fps: f32,
    pub error_message: Option<String>,
}

impl Default for StreamStatus {
    fn default() -> Self {
        Self {
            is_running: false,
            device_online: false,
            target_width: 640,
            target_height: 480,
            current_file: String::new(),
            pushed_frames: 0,
            current_fps: 0.0,
            error_message: None,
        }
    }
}

pub struct StreamManager {
    status: Arc<Mutex<StreamStatus>>,
    stop_signal: Arc<AtomicBool>,
    worker_handle: Option<thread::JoinHandle<()>>,
    cached_specs: Arc<Mutex<(u32, u32)>>,
    cached_device_path: Arc<Mutex<Option<String>>>,
    child_handle: Arc<Mutex<Option<Child>>>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StreamStatus::default())),
            stop_signal: Arc::new(AtomicBool::new(false)),
            worker_handle: None,
            cached_specs: Arc::new(Mutex::new((640, 480))),
            cached_device_path: Arc::new(Mutex::new(None)),
            child_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 获取当前推流运行状态
    pub fn get_status(&self) -> StreamStatus {
        let mut s = self.status.lock().unwrap().clone();
        if s.is_running {
            s.device_online = find_device_path().is_ok();
        }
        let specs = *self.cached_specs.lock().unwrap();
        s.target_width = specs.0;
        s.target_height = specs.1;
        s
    }

    /// 探测或获取设备的实际规格尺寸
    pub fn get_device_specs(&self) -> (u32, u32, bool) {
        let is_running = self.status.lock().unwrap().is_running;
        if is_running {
            let specs = *self.cached_specs.lock().unwrap();
            let online = self.status.lock().unwrap().device_online;
            return (specs.0, specs.1, online);
        }

        let path = match find_device_path() {
            Ok(path) => path,
            Err(_) => {
                self.status.lock().unwrap().device_online = false;
                let specs = *self.cached_specs.lock().unwrap();
                return (specs.0, specs.1, false);
            }
        };

        let already_probed = self
            .cached_device_path
            .lock()
            .unwrap()
            .as_ref()
            .map(|cached| cached == &path)
            .unwrap_or(false)
            && self.status.lock().unwrap().device_online;
        if already_probed {
            let specs = *self.cached_specs.lock().unwrap();
            return (specs.0, specs.1, true);
        }

        match UsbDevice::open() {
            Ok(dev) => {
                let geom = dev.read_geometry().unwrap_or(DeviceGeometry {
                    width: 640,
                    height: 480,
                    smem_len: 614400,
                });
                let mut c = self.cached_specs.lock().unwrap();
                *c = (geom.width, geom.height);
                *self.cached_device_path.lock().unwrap() = Some(path);
                self.status.lock().unwrap().device_online = true;
                (c.0, c.1, true)
            }
            Err(_) => {
                self.status.lock().unwrap().device_online = false;
                let specs = *self.cached_specs.lock().unwrap();
                (specs.0, specs.1, false)
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
            let _ = handle.join();
        }
        self.child_handle.lock().unwrap().take();
        let mut st = self.status.lock().unwrap();
        st.is_running = false;
        st.device_online = false;
        st.current_fps = 0.0;
    }

    /// 启动推流任务
    pub fn start(&mut self, config: StreamConfig) -> Result<(), String> {
        // 先停掉旧任务
        self.stop();

        // 查找 ffmpeg 可执行文件路径
        let ffmpeg_path = resolve_ffmpeg(config.custom_ffmpeg.as_deref())?;

        // 检查媒体文件是否存在
        let media_path = Path::new(&config.file_path);
        if !media_path.exists() {
            return Err(format!("媒体文件不存在: {}", config.file_path));
        }

        // 尝试打开 WinUSB 设备
        let device = UsbDevice::open()?;

        self.stop_signal.store(false, Ordering::SeqCst);
        let stop_flag = Arc::clone(&self.stop_signal);
        let status_arc = Arc::clone(&self.status);
        let cached_specs_arc = Arc::clone(&self.cached_specs);
        let child_handle = Arc::clone(&self.child_handle);

        {
            let mut st = status_arc.lock().unwrap();
            st.is_running = true;
            st.device_online = true;
            st.current_file = config.file_path.clone();
            st.pushed_frames = 0;
            st.current_fps = 0.0;
            st.error_message = None;
        }

        let handle = thread::spawn(move || {
            run_stream_worker(
                device,
                ffmpeg_path,
                config,
                stop_flag,
                status_arc,
                cached_specs_arc,
                child_handle,
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

/// 构造 FFmpeg 视频滤镜字符串 (支持动态目标宽高)
fn build_filter_string(config: &StreamConfig, target_w: u32, target_h: u32) -> String {
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
            // custom 或自由模式：缩放到目标屏分辨率
            parts.push(format!(
                "scale={}:{}:flags=fast_bilinear",
                target_w, target_h
            ));
        }
    }

    parts.join(",")
}

/// 启动 FFmpeg 子进程并返回管道
fn spawn_ffmpeg(
    ffmpeg_path: &Path,
    config: &StreamConfig,
    target_w: u32,
    target_h: u32,
) -> Result<Child, String> {
    let filter = build_filter_string(config, target_w, target_h);
    let fps_str = config.fps.max(5).min(60).to_string();

    let mut cmd = Command::new(ffmpeg_path);
    cmd.arg("-v").arg("error");

    if config.is_loop {
        cmd.arg("-stream_loop").arg("-1");
    }

    cmd.arg("-re")
        .arg("-threads")
        .arg("2")
        // FFmpeg 默认会按 CPU 核数创建滤镜线程池；本项目只有缩放/裁切，
        // 单线程已足够实时，可避免空闲核心被滤镜池唤醒。
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
        .arg("bgr565le")
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

/// 实际执行推流的工作线程
fn run_stream_worker(
    device: UsbDevice,
    ffmpeg_path: PathBuf,
    config: StreamConfig,
    stop_flag: Arc<AtomicBool>,
    status_arc: Arc<Mutex<StreamStatus>>,
    cached_specs_arc: Arc<Mutex<(u32, u32)>>,
    child_handle: Arc<Mutex<Option<Child>>>,
) {
    if stop_flag.load(Ordering::SeqCst) {
        return;
    }

    // 1. 冷待机双轮初始化唤醒并获取硬件自报分辨率
    let geom = match device.init_screen() {
        Ok(g) => g,
        Err(e) => {
            let mut st = status_arc.lock().unwrap();
            st.is_running = false;
            st.device_online = false;
            st.error_message = Some(format!("屏幕唤醒初始化失败: {}", e));
            return;
        }
    };

    // 硬件自报尺寸是协议的权威值。前端传入的尺寸只有在与硬件一致时才采用，
    // 避免设备探测暂时失败时把默认 640x480 错传给其它面板。
    let (target_w, target_h) = match (config.target_width, config.target_height) {
        (Some(w), Some(h)) if w == geom.width && h == geom.height => (w, h),
        _ => (geom.width, geom.height),
    };
    {
        let mut c = cached_specs_arc.lock().unwrap();
        *c = (target_w, target_h);
    }

    let frame_bytes = (target_w * target_h * 2) as usize;
    let mut buf = vec![0u8; frame_bytes];
    let mut last_frame = vec![0u8; frame_bytes];
    let mut has_frame = false;
    let mut pushed_count: u64 = 0;

    let target_fps = config.fps.max(5).min(60);
    let frame_interval = Duration::from_millis(1000 / target_fps as u64);

    let mut stdout = match spawn_ffmpeg(&ffmpeg_path, &config, target_w, target_h)
        .and_then(|child| install_ffmpeg_child(child, &child_handle))
    {
        Ok(stdout) => stdout,
        Err(e) => {
            let mut st = status_arc.lock().unwrap();
            st.is_running = false;
            st.error_message = Some(e);
            return;
        }
    };

    let mut last_stat_time = Instant::now();
    let mut stat_frames = 0u64;

    while !stop_flag.load(Ordering::SeqCst) {
        let frame_start = Instant::now();
        let mut got = 0;
        let mut eof = false;

        // 从 stdout 读取完整的一帧 (target_w * target_h * 2 字节)
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

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        if !eof && got == frame_bytes {
            // 交换两个复用缓冲区，避免每帧复制整块像素数据；last_frame
            // 仍然保留给静态图片/非循环视频做持续保活。
            std::mem::swap(&mut buf, &mut last_frame);
            has_frame = true;
        } else {
            // 输入流结束（例如静态单张图片、或短动图非 loop 模式）
            if !has_frame {
                let mut st = status_arc.lock().unwrap();
                st.is_running = false;
                st.error_message = Some("未解码到有效画面（文件损坏或格式不支持）".to_string());
                kill_ffmpeg_child(&child_handle);
                return;
            }

            if config.is_loop {
                // 重启 ffmpeg
                kill_ffmpeg_child(&child_handle);
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
                match spawn_ffmpeg(&ffmpeg_path, &config, target_w, target_h)
                    .and_then(|child| install_ffmpeg_child(child, &child_handle))
                {
                    Ok(new_stdout) => stdout = new_stdout,
                    Err(e) => {
                        let mut st = status_arc.lock().unwrap();
                        st.is_running = false;
                        st.error_message = Some(format!("循环重启 ffmpeg 失败: {}", e));
                        return;
                    }
                }
            }
        }

        // 推送最后一帧有效数据
        if has_frame {
            if let Err(e) = device.push_frame(&last_frame, target_w, target_h) {
                let mut st = status_arc.lock().unwrap();
                st.is_running = false;
                st.error_message = Some(format!("推流中断: {}", e));
                kill_ffmpeg_child(&child_handle);
                return;
            }
            pushed_count += 1;
            stat_frames += 1;
        }

        // 帧率控制 (若推流过快则平滑 sleep)
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }

        // 每秒更新一次状态
        if last_stat_time.elapsed() >= Duration::from_secs(1) {
            let actual_fps = stat_frames as f32 / last_stat_time.elapsed().as_secs_f32();
            let mut st = status_arc.lock().unwrap();
            st.pushed_frames = pushed_count;
            st.current_fps = actual_fps;
            st.target_width = target_w;
            st.target_height = target_h;
            last_stat_time = Instant::now();
            stat_frames = 0;
        }
    }

    kill_ffmpeg_child(&child_handle);
    let mut st = status_arc.lock().unwrap();
    st.is_running = false;
    st.current_fps = 0.0;
}
