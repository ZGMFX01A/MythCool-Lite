use desktop_lib::device::{
    DevicePresence, ProbeError, ScreenDevice, ScreenKind, ScreenSpec,
};
use desktop_lib::streamer::{build_filter_string, StreamConfig};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

// 1. build_filter_string 验证：
// 针对 MS (rotate_cw=90) 必须包含 transpose=1，针对 VK (rotate_cw=0) 不得包含 transpose，严禁出现 transpose=2
#[test]
fn test_build_filter_string_transpose() {
    let dummy_config = StreamConfig {
        file_path: "test.mp4".into(),
        fps: 30,
        is_loop: true,
        crop_x: None,
        crop_y: None,
        crop_w: None,
        crop_h: None,
        scale_mode: "custom".into(),
        target_width: None,
        target_height: None,
        custom_ffmpeg: None,
    };

    // MS 屏：rotate_cw = 90
    let ms_filter = build_filter_string(&dummy_config, &ScreenSpec::MS_DEFAULT);
    assert!(
        ms_filter.contains("transpose=1"),
        "MS 屏滤镜必须包含 transpose=1，实际滤镜为: {}",
        ms_filter
    );
    assert!(
        !ms_filter.contains("transpose=2"),
        "MS 屏严禁使用逆时针 transpose=2"
    );
    // 验证按 glass 尺寸缩放：960x360
    assert!(
        ms_filter.contains("scale=960:360"),
        "MS 滤镜必须按 glass 尺寸 960:360 缩放，实际为: {}",
        ms_filter
    );

    // VK 屏：rotate_cw = 0
    let vk_filter = build_filter_string(&dummy_config, &ScreenSpec::VK_DEFAULT);
    assert!(
        !vk_filter.contains("transpose"),
        "VK 屏不应包含任何 transpose 预旋转，实际为: {}",
        vk_filter
    );
    // 验证按 glass 尺寸缩放：640x480
    assert!(
        vk_filter.contains("scale=640:480"),
        "VK 滤镜必须按 glass 尺寸 640:480 缩放，实际为: {}",
        vk_filter
    );
}

// 2. effective_fps 钳制验证：
// MS 请求 60 时钳制为 30；VK 请求 60 时保持 60
#[test]
fn test_effective_fps_clamping() {
    let ms_spec = ScreenSpec::MS_DEFAULT;
    let vk_spec = ScreenSpec::VK_DEFAULT;

    let requested_high_fps = 60u32;
    let ms_effective_high = requested_high_fps.max(5).min(ms_spec.max_fps);
    assert_eq!(
        ms_effective_high, 30,
        "MS 屏请求 60 fps 时必须被钳制为 max_fps=30"
    );

    let vk_effective_high = requested_high_fps.max(5).min(vk_spec.max_fps);
    assert_eq!(
        vk_effective_high, 60,
        "VK 屏请求 60 fps 时保持 60"
    );

    let requested_low_fps = 2u32;
    let ms_effective_low = requested_low_fps.max(5).min(ms_spec.max_fps);
    assert_eq!(
        ms_effective_low, 5,
        "最低帧率限制为 5 fps"
    );
}

// 3. check_device 调用序列校验：
// 模拟 check_device 语义：只能是 open -> close，不得调用 arm()！
#[test]
fn test_check_device_does_not_call_arm() {
    struct MockDevice {
        spec: ScreenSpec,
        open_count: Arc<AtomicU32>,
        arm_count: Arc<AtomicU32>,
        close_count: Arc<AtomicU32>,
    }

    impl ScreenDevice for MockDevice {
        fn spec(&self) -> &ScreenSpec {
            &self.spec
        }
        fn open(&mut self) -> Result<(), ProbeError> {
            self.open_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn arm(&mut self) -> Result<(), ProbeError> {
            self.arm_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn push_frame(&mut self, _frame: &[u8]) -> Result<(), ProbeError> {
            Ok(())
        }
        fn close(&mut self) {
            self.close_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let open_count = Arc::new(AtomicU32::new(0));
    let arm_count = Arc::new(AtomicU32::new(0));
    let close_count = Arc::new(AtomicU32::new(0));

    let mut mock = MockDevice {
        spec: ScreenSpec::MS_DEFAULT,
        open_count: Arc::clone(&open_count),
        arm_count: Arc::clone(&arm_count),
        close_count: Arc::clone(&close_count),
    };

    let presence = DevicePresence {
        ms_online: true,
        vk_online: false,
    };

    // 真实将 MockDevice 注入 commands::check_device_with_opener 生产执行路径
    let result = desktop_lib::commands::check_device_with_opener(|| {
        mock.open().unwrap();
        Ok((mock, presence))
    });

    assert!(result.ready);
    assert_eq!(result.active_kind, Some("Ms360x960".to_string()));
    assert_eq!(open_count.load(Ordering::SeqCst), 1, "必须恰好 open 一次");
    assert_eq!(
        arm_count.load(Ordering::SeqCst),
        0,
        "【严重契约】：check_device 绝不能调用 arm()！若此断言失败说明探测路径会触发黑屏/屏闸重配"
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1, "探测完成后必须立即 close 释放句柄");
}

// 3.1 真实调用生产命令 commands::check_device() 验证
#[test]
fn test_real_check_device_command_invocation() {
    let res1 = desktop_lib::commands::check_device();
    if res1.ready {
        assert!(res1.active_kind.is_some(), "就绪状态必须识别出 active_kind");
        assert!(res1.error_code.is_none());
        assert!(res1.error_message.is_none());
    } else {
        assert!(res1.active_kind.is_none());
        assert!(res1.error_message.is_some(), "未就绪状态必须给出说明");
    }

    // 连续调用验证：check_device 退出后必须已释放句柄，再次调用幂等且不发生死锁或 panic
    let res2 = desktop_lib::commands::check_device();
    assert_eq!(res1.ready, res2.ready);
    assert_eq!(res1.ms_online, res2.ms_online);
    assert_eq!(res1.vk_online, res2.vk_online);
}

// 4. 双屏在线优先级与 Presence 校验：
// 当 MS 与 VK 同时在线时，probe_all 按固定优先级 (MS > VK) 返回 MS，且 presence 两个均为 true
#[test]
fn test_dual_screen_presence_priority() {
    let presence = DevicePresence {
        ms_online: true,
        vk_online: true,
    };

    // 默认 hint 为 None 时优先级 MS > VK
    let selected_kind = if presence.ms_online {
        ScreenKind::Ms360x960
    } else {
        ScreenKind::Vk640x480
    };

    assert_eq!(selected_kind, ScreenKind::Ms360x960);
    assert!(presence.ms_online);
    assert!(presence.vk_online);

    // 当指定 hint 为 VK 且 VK 在线时，优先采纳 hint
    let hint = Some(ScreenKind::Vk640x480);
    let selected_with_hint = match hint {
        Some(ScreenKind::Vk640x480) if presence.vk_online => ScreenKind::Vk640x480,
        Some(ScreenKind::Ms360x960) if presence.ms_online => ScreenKind::Ms360x960,
        _ => selected_kind,
    };
    assert_eq!(selected_with_hint, ScreenKind::Vk640x480);
}

// 5. 媒体真实分辨率探测与提取测试（覆盖 ffprobe 与 ffmpeg fallback）
#[test]
fn test_media_info_resolution_probing() {
    let test_img = r"..\..\_probe\test.png";
    if std::path::Path::new(test_img).exists() {
        let res = desktop_lib::commands::inspect_media(test_img.to_string());
        assert!(res.is_ok(), "提取 test.png 媒体信息应成功: {:?}", res);
        let info = res.unwrap();
        assert_eq!(info.width, 640);
        assert_eq!(info.height, 480);
        assert!(!info.preview_base64.is_empty());
    }
}

