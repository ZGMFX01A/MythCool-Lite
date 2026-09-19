//! Windows 系统关机与注销生命周期监听及错误抑制模块
//! 负责在 Windows 关机时提前捕获 WM_QUERYENDSESSION / WM_ENDSESSION，
//! 及时中断推流并终止 FFmpeg 进程，防止关机阶段 FFmpeg 产生内存访问异常弹窗（0xc0000005）。

use crate::streamer::StreamManager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 系统是否正在关机或注销的全局原子标记
/// 用于通知工作线程禁止重启 FFmpeg，并允许主事件循环退出应用。
static SHUTDOWN_SIGNALED: AtomicBool = AtomicBool::new(false);

/// 全局持有的推流管理器弱引用/共享句柄，供 Win32 消息循环回调访问
static SHUTDOWN_MANAGER: OnceLock<Arc<Mutex<StreamManager>>> = OnceLock::new();

/// 查询当前是否处于关机或注销流程中
pub fn is_shutting_down() -> bool {
    SHUTDOWN_SIGNALED.load(Ordering::SeqCst)
}

/// 设置关机状态标记
#[allow(dead_code)]
pub fn set_shutting_down(val: bool) {
    SHUTDOWN_SIGNALED.store(val, Ordering::SeqCst);
}

/// 初始化关机监听器与错误抑制模式
#[cfg(windows)]
pub fn init_windows_shutdown_handler(stream_manager: Arc<Mutex<StreamManager>>) {
    let _ = SHUTDOWN_MANAGER.set(stream_manager);

    unsafe {
        // 抑制 Windows 错误报告（WER）弹窗：
        // SEM_FAILCRITICALERRORS (0x0001): 不显示严重错误弹窗
        // SEM_NOGPFAULTERRORBOX (0x0002): 不显示应用程序崩溃/访问违规（如 0xc0000005 该内存不能为read）弹窗
        // SEM_NOOPENFILEERRORBOX (0x8000): 不显示文件未找到弹窗
        // 子进程（包括 FFmpeg）会默认继承该错误模式，确保极端情况下即使子进程异常也绝不弹窗打扰用户。
        win_ffi::SetErrorMode(0x0001 | 0x0002 | 0x8000);

        // 设置当前进程在 Windows 关机序列中的优先级为 0x3FF（普通应用程序最高优先级，0x400+ 为系统级进程）。
        // 确保在系统关机初期优先收到关机广播，在驱动、网络和子进程被强杀前完成清理。
        win_ffi::SetProcessShutdownParameters(0x3FF, 0);
    }

    // 启动专职线程维持顶级不可见窗口与消息泵，专门接收广播系统消息
    std::thread::Builder::new()
        .name("windows-shutdown-listener".to_string())
        .spawn(move || {
            unsafe {
                let class_name = encode_wide("MythCoolLiteShutdownListener");
                let mut wc: win_ffi::WNDCLASSEXW = std::mem::zeroed();
                wc.cbSize = std::mem::size_of::<win_ffi::WNDCLASSEXW>() as u32;
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = win_ffi::GetModuleHandleW(std::ptr::null());
                wc.lpszClassName = class_name.as_ptr();

                let atom = win_ffi::RegisterClassExW(&wc);
                if atom == 0 {
                    eprintln!("注册关机监听窗口类失败 (系统消息监听未就绪)");
                    return;
                }

                // 创建顶级不可见窗口（注意：不能是 HWND_MESSAGE，因为 Windows 不向消息窗口广播 WM_QUERYENDSESSION）
                let hwnd = win_ffi::CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    win_ffi::WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    wc.hInstance,
                    std::ptr::null_mut(),
                );

                if hwnd == 0 {
                    eprintln!("创建关机监听顶级窗口失败");
                    return;
                }

                // 标准 Win32 消息泵
                let mut msg = std::mem::zeroed();
                while win_ffi::GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    win_ffi::TranslateMessage(&msg);
                    win_ffi::DispatchMessageW(&msg);
                }
            }
        })
        .expect("启动关机监听线程失败");
}

#[cfg(not(windows))]
pub fn init_windows_shutdown_handler(_stream_manager: Arc<Mutex<StreamManager>>) {}

#[cfg(windows)]
fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
unsafe extern "system" fn wndproc(
    hwnd: win_ffi::HWND,
    msg: u32,
    wparam: win_ffi::WPARAM,
    lparam: win_ffi::LPARAM,
) -> win_ffi::LRESULT {
    match msg {
        win_ffi::WM_QUERYENDSESSION => {
            // Windows 正在询问是否准备好关机
            SHUTDOWN_SIGNALED.store(true, Ordering::SeqCst);
            if let Some(mgr_arc) = SHUTDOWN_MANAGER.get() {
                if let Ok(mut mgr) = mgr_arc.lock() {
                    // 立即终止 FFmpeg 与推流线程，释放 USB 设备
                    mgr.stop();
                }
            }
            1 // 返回 1 (TRUE)，告知 Windows 准备完毕同意关机
        }
        win_ffi::WM_ENDSESSION => {
            if wparam != 0 {
                // Windows 确定执行关机/重启/注销
                SHUTDOWN_SIGNALED.store(true, Ordering::SeqCst);
                if let Some(mgr_arc) = SHUTDOWN_MANAGER.get() {
                    if let Ok(mut mgr) = mgr_arc.lock() {
                        mgr.stop();
                    }
                }
                // 立即干净退出进程，避免被 Windows 5 秒超时强制杀死
                std::process::exit(0);
            } else {
                // 用户或系统取消了关机
                SHUTDOWN_SIGNALED.store(false, Ordering::SeqCst);
            }
            0
        }
        _ => win_ffi::DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(windows)]
#[allow(non_snake_case, dead_code)]
mod win_ffi {
    pub type HWND = isize;
    pub type HINSTANCE = isize;
    pub type HICON = isize;
    pub type HCURSOR = isize;
    pub type HBRUSH = isize;
    pub type ATOM = u16;
    pub type WPARAM = usize;
    pub type LPARAM = isize;
    pub type LRESULT = isize;

    /// Windows 关机询问消息
    pub const WM_QUERYENDSESSION: u32 = 0x0011;
    /// Windows 关机执行或取消消息
    pub const WM_ENDSESSION: u32 = 0x0016;
    /// 标准顶层重叠窗口样式（不含 WS_VISIBLE，保证窗口不可见）
    pub const WS_OVERLAPPED: u32 = 0x00000000;

    #[repr(C)]
    pub struct WNDCLASSEXW {
        pub cbSize: u32,
        pub style: u32,
        pub lpfnWndProc: Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
        pub cbClsExtra: i32,
        pub cbWndExtra: i32,
        pub hInstance: HINSTANCE,
        pub hIcon: HICON,
        pub hCursor: HCURSOR,
        pub hbrBackground: HBRUSH,
        pub lpszMenuName: *const u16,
        pub lpszClassName: *const u16,
        pub hIconSm: HICON,
    }

    #[repr(C)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    pub struct MSG {
        pub hwnd: HWND,
        pub message: u32,
        pub wParam: WPARAM,
        pub lParam: LPARAM,
        pub time: u32,
        pub pt: POINT,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn SetErrorMode(uMode: u32) -> u32;
        pub fn SetProcessShutdownParameters(dwLevel: u32, dwFlags: u32) -> i32;
        pub fn GetModuleHandleW(lpModuleName: *const u16) -> HINSTANCE;
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> ATOM;
        pub fn CreateWindowExW(
            dwExStyle: u32,
            lpClassName: *const u16,
            lpWindowName: *const u16,
            dwStyle: u32,
            X: i32,
            Y: i32,
            nWidth: i32,
            nHeight: i32,
            hWndParent: HWND,
            hMenu: isize,
            hInstance: HINSTANCE,
            lpParam: *mut std::ffi::c_void,
        ) -> HWND;
        pub fn DefWindowProcW(hWnd: HWND, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
        pub fn GetMessageW(lpMsg: *mut MSG, hWnd: HWND, wMsgFilterMin: u32, wMsgFilterMax: u32) -> i32;
        pub fn TranslateMessage(lpMsg: *const MSG) -> i32;
        pub fn DispatchMessageW(lpMsg: *const MSG) -> LRESULT;
    }
}
