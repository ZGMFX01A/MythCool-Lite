//! Public adapter for the private Myth.Cool device core.
//!
//! The application depends on this small surface instead of importing the
//! device-specific WinUSB/libusb implementation throughout the codebase.

use std::sync::Arc;

#[allow(unused_imports)]
pub use mythcool_device_core::{
    find_device_path, open_candidate, probe_all, DeviceCandidate, DeviceGeometry, DevicePresence,
    Diag, Noop, ProbeError, ProbeResult, ScreenDevice, ScreenKind, ScreenSpec, UsbDevice,
};

/// 探测并打开当前活跃设备（不调用 arm，仅完成无副作用的 open）
pub fn open_active_device(
    diag: Arc<dyn Diag>,
) -> Result<(Box<dyn ScreenDevice>, DevicePresence), ProbeError> {
    let probe = probe_all(None)?;
    let dev = open_candidate(&probe.selected, diag)?;
    Ok((dev, probe.presence))
}
