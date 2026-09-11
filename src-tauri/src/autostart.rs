//! Windows 开机自启管理模块
//! 使用注册表项 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 写入与读取

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const APP_NAME: &str = "MythCoolLite";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 获取当前开机自启状态
pub fn is_autostart_enabled() -> Result<bool, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = match hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_READ) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };

    match run_key.get_value::<String, _>(APP_NAME) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// 启用或禁用开机自启
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run_key, _) = hkcu
        .create_subkey_with_flags(RUN_KEY_PATH, KEY_WRITE | KEY_READ)
        .map_err(|e| format!("无法打开注册表启动项: {}", e))?;

    if enabled {
        let exe_path =
            std::env::current_exe().map_err(|e| format!("获取当前可执行文件路径失败: {}", e))?;
        let cmd = format!("\"{}\" --minimized", exe_path.to_string_lossy());
        run_key
            .set_value(APP_NAME, &cmd)
            .map_err(|e| format!("写入自启注册表失败: {}", e))?;
    } else {
        let _ = run_key.delete_value(APP_NAME);
    }

    Ok(())
}
