//! Public adapter for the private Myth.Cool device core.
//!
//! The application depends on this small surface instead of importing the
//! device-specific WinUSB implementation throughout the codebase.

pub use mythcool_device_core::{find_device_path, DeviceGeometry, UsbDevice};
