# Myth.Cool Lite

专为 VK 带屏水冷设计的轻量副屏播放工具。

## 为什么做它

原版 Myth.Cool 常驻进程多、占用资源高。Myth.Cool Lite 只保留副屏播放所需功能，在实际测试中明显降低了电脑负载：

| 项目 | 原版 Myth.Cool | Myth.Cool Lite |
| --- | ---: | ---: |
| 进程数 | 9 | 4 |
| 内存占用 | 864 MB | 184 MB |
| CPU 占用（单核） | 26.3% | 3.2% |

## 界面预览

![Myth.Cool Lite 界面预览](assets/running.png)

## 下载

前往 [GitHub Releases](https://github.com/ZGMFX01A/MythCool-Lite/releases) 下载单体程序 `MythCoolLite.exe`（约 143 MB，已内置完整 FFmpeg 解码环境，开箱即用），无需安装，无需配置任何外部依赖。

## 适用范围与设备兼容性

* **适用设备**：适用于 VK（瓦尔基里）带屏、且屏幕本身无独立存储与播放芯片、依赖主机 USB 推流的水冷设备。
* **分辨率动态自适应**：程序具备**硬件分辨率自动探测与推流自适应**能力。基准验证型号为 `MC360`（640×480），但同时向下兼容任意分辨率规格（如 360×960、480×480 等长条屏/方屏），程序会自动读取面板物理规格并动态裁切与缩放。
* **驱动与型号适配**：
  * **当前正式版**：支持基于 Windows 原生 WinUSB 驱动的型号（如 VID `374A` / PID `A021`）。
  * **扩展适配中**：针对 `345F:9132`（MS USB Display / libusb-win32 驱动栈）等复合设备型号，正在引入 libusb-1.0 统一驱动栈，详见 [Issue #1](https://github.com/ZGMFX01A/MythCool-Lite/issues/1)。

## 使用注意事项

* **设备独占**：使用前请**完全退出原版 Myth.Cool（包含系统托盘进程）**，否则 USB 设备句柄会被原版独占导致连接失败。
