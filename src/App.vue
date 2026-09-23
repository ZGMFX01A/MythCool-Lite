<script setup lang="ts">
import { ref, onMounted, onUnmounted, computed, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow } from "@tauri-apps/api/window";
import appIcon from "../src-tauri/icons/128x128.png";
import { PUBLIC_REPOSITORY_URL } from "./repository";

// 硬件与推流状态
const deviceOnline = ref(false);
const activeKind = ref<string>("");
const msOnline = ref(false);
const vkOnline = ref(false);
const deviceWidth = ref(640);
const deviceHeight = ref(480);
const deviceAspectStr = ref("4:3");
const deviceMaxFps = ref(60);

const isStreaming = ref(false);
const currentFps = ref(0);
const effectiveFps = ref(16);
const errorMessage = ref("");
const isAutostart = ref(false);
const publicRepositoryUrl = PUBLIC_REPOSITORY_URL;
const selectedMediaStorageKey = "mythcool-lite.selected-media.v1";

const isDeviceReady = ref(false);
const isDeviceBusy = ref(false);

const deviceStatusText = computed(() => {
  if (isStreaming.value) {
    if (msOnline.value && vkOnline.value) {
      const activeLabel = activeKind.value.includes("Ms") ? "MS机箱屏" : "VK水冷屏";
      return `双屏推流中 (${activeLabel}, ${deviceWidth.value}×${deviceHeight.value})`;
    }
    if (msOnline.value) {
      return `MS 机箱屏推流中 (${deviceWidth.value}×${deviceHeight.value})`;
    }
    if (vkOnline.value) {
      return `VK 水冷屏推流中 (${deviceWidth.value}×${deviceHeight.value})`;
    }
    return `推流中 (${deviceWidth.value}×${deviceHeight.value})`;
  }

  if (isDeviceBusy.value) {
    return "副屏已被官方应用占用 (请退出原版 Myth.Cool)";
  }

  if (msOnline.value && vkOnline.value) {
    const activeLabel = activeKind.value.includes("Ms") ? "MS机箱屏" : "VK水冷屏";
    const readyStr = isDeviceReady.value ? "已就绪" : "已检测到";
    return `双屏在线 (${readyStr}, 优先: ${activeLabel}, ${deviceWidth.value}×${deviceHeight.value})`;
  }
  if (msOnline.value) {
    const readyStr = isDeviceReady.value ? "已就绪" : "已检测到";
    return `MS 机箱屏${readyStr} (${deviceWidth.value}×${deviceHeight.value})`;
  }
  if (vkOnline.value) {
    const readyStr = isDeviceReady.value ? "已就绪" : "已检测到";
    return `VK 水冷屏${readyStr} (${deviceWidth.value}×${deviceHeight.value})`;
  }
  return "副屏未连接";
});

const colorChannelText = computed(() => (activeKind.value.includes("Ms") ? "BGR888" : "BGR565"));

// 媒体与裁切参数
const selectedFile = ref<string>("");
const fileName = computed(() => {
  if (!selectedFile.value) return "";
  const parts = selectedFile.value.split(/[\\/]/);
  return parts[parts.length - 1];
});
const originalWidth = ref(640);
const originalHeight = ref(480);
const isMediaLoaded = ref(false);
const isLoadingMedia = ref(false);

// 推流设置
const fps = ref(16);
const isLoop = ref(true);
const scaleMode = ref<"custom" | "cover" | "contain" | "stretch">("custom");
const lockAspect = ref(true); // 锁定当前副屏硬件比例

// 裁切框参数 (原始像素坐标)
const crop = ref({
  x: 0,
  y: 0,
  w: 640,
  h: 480,
});

// Canvas 交互引用
const canvasRef = ref<HTMLCanvasElement | null>(null);
let previewImage: HTMLImageElement | null = null;
let pollTimer: number | null = null;
let unlistenDrop: (() => void) | null = null;
let autoStartPending = false;
let autoStartInProgress = false;

// 拖拽手柄状态
type DragHandle = "none" | "inside" | "nw" | "ne" | "sw" | "se" | "n" | "s" | "w" | "e";
let activeHandle: DragHandle = "none";
let startX = 0;
let startY = 0;
let startCrop = { x: 0, y: 0, w: 0, h: 0 };

// 周期性轮询推流状态与硬件规格
async function refreshStatus() {
  try {
    const status: any = await invoke("get_stream_status");
    isStreaming.value = status.is_running;
    deviceOnline.value = status.device_online;
    currentFps.value = status.current_fps;
    effectiveFps.value = status.effective_fps || status.requested_fps || fps.value;
    if (status.last_error) {
      errorMessage.value = status.last_error;
    } else if (status.error_message) {
      errorMessage.value = status.error_message;
    }

    // 获取并更新设备实际硬件分辨率和比例
    const specs: any = await invoke("get_device_specs");
    if (specs) {
      msOnline.value = specs.ms_online === true;
      vkOnline.value = specs.vk_online === true;
      deviceOnline.value = specs.online === true;
      activeKind.value = specs.active_kind || "";
      deviceMaxFps.value = specs.max_fps || 60;

      if (specs.width && specs.height) {
        const changed = deviceWidth.value !== specs.width || deviceHeight.value !== specs.height;
        deviceWidth.value = specs.width;
        deviceHeight.value = specs.height;
        deviceAspectStr.value = specs.aspect_ratio_str || `${specs.width}:${specs.height}`;

        if (changed && lockAspect.value && isMediaLoaded.value) {
          resetCrop();
        }
      }

      // 未推流且检测到硬件在线时，执行 readiness 校验以识别占用
      if (!isStreaming.value && (specs.ms_online || specs.vk_online)) {
        try {
          const chk: any = await invoke("check_device");
          if (chk) {
            isDeviceReady.value = chk.ready === true;
            isDeviceBusy.value = chk.error_code === -5;
          }
        } catch (_) {
          // 保持检测状态
        }
      } else if (isStreaming.value) {
        isDeviceReady.value = true;
        isDeviceBusy.value = false;
      }
    }

    if (autoStartPending) {
      await tryAutoStart();
    }
  } catch (e) {
    console.error("查询状态失败:", e);
  }
}

// 切换自启动
async function toggleAutostart() {
  try {
    await invoke("set_autostart", { enabled: isAutostart.value });
  } catch (e: any) {
    alert("修改自启动失败: " + e);
    isAutostart.value = !isAutostart.value;
  }
}

async function openPublicRepository() {
  try {
    await openUrl(publicRepositoryUrl);
  } catch (e: any) {
    errorMessage.value = "打开项目主页失败: " + e;
  }
}

// 选择文件
async function selectMediaFile() {
  try {
    const file = await open({
      multiple: false,
      filters: [
        {
          name: "多媒体文件 (视频/图片/GIF)",
          extensions: ["mp4", "mkv", "avi", "mov", "webm", "gif", "jpg", "jpeg", "png", "bmp", "webp"],
        },
      ],
    });

    if (file && typeof file === "string") {
      await loadMedia(file);
    }
  } catch (e: any) {
    errorMessage.value = "选择文件失败: " + e;
  }
}

// 加载并解析媒体
async function loadMedia(filePath: string) {
  selectedFile.value = filePath;
  isLoadingMedia.value = true;
  errorMessage.value = "";
  isMediaLoaded.value = false;
  previewImage = null;

  try {
    const info: any = await invoke("inspect_media", { filePath });
    originalWidth.value = info.width || 640;
    originalHeight.value = info.height || 480;
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => {
        previewImage = img;
        isMediaLoaded.value = true;
        isLoadingMedia.value = false;
        try {
          localStorage.setItem(selectedMediaStorageKey, filePath);
        } catch {
          // 本地存储不可用时不影响当前会话使用。
        }
        resetCrop();
        drawCanvas();
        resolve();
      };
      img.onerror = () => reject(new Error("预览图加载失败"));
      img.src = info.preview_base64;
    });
  } catch (e: any) {
    selectedFile.value = "";
    isLoadingMedia.value = false;
    try {
      localStorage.removeItem(selectedMediaStorageKey);
    } catch {
      // 本地存储不可用时忽略清理失败。
    }
    errorMessage.value = "提取媒体预览失败: " + e;
  }
}

// 重置裁切框：自动居中并适配当前设备的物理分辨率比例
function resetCrop() {
  const targetRatio = deviceWidth.value / deviceHeight.value;
  const origW = originalWidth.value;
  const origH = originalHeight.value;
  const currentRatio = origW / origH;

  let w = origW;
  let h = origH;

  if (currentRatio > targetRatio) {
    // 宽屏，两边裁切
    w = Math.round(origH * targetRatio);
    h = origH;
  } else {
    // 窄屏，上下裁切
    w = origW;
    h = Math.round(origW / targetRatio);
  }

  const x = Math.max(0, Math.round((origW - w) / 2));
  const y = Math.max(0, Math.round((origH - h) / 2));

  crop.value = { x, y, w, h };
  drawCanvas();
}

// 转换为完整画面无裁切
function fitFullCrop() {
  crop.value = {
    x: 0,
    y: 0,
    w: originalWidth.value,
    h: originalHeight.value,
  };
  drawCanvas();
}

// 绘制画布
function drawCanvas() {
  const canvas = canvasRef.value;
  if (!canvas || !previewImage) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const w = canvas.width;
  const h = canvas.height;

  // 清除背景
  ctx.clearRect(0, 0, w, h);

  // 缩放绘制底图
  ctx.drawImage(previewImage, 0, 0, w, h);

  // 坐标映射比率
  const scaleX = w / originalWidth.value;
  const scaleY = h / originalHeight.value;

  const cx = crop.value.x * scaleX;
  const cy = crop.value.y * scaleY;
  const cw = crop.value.w * scaleX;
  const ch = crop.value.h * scaleY;

  // 绘制暗色半透明蒙版
  ctx.fillStyle = "rgba(0, 0, 0, 0.55)";
  ctx.fillRect(0, 0, w, cy); // 上
  ctx.fillRect(0, cy + ch, w, h - (cy + ch)); // 下
  ctx.fillRect(0, cy, cx, ch); // 左
  ctx.fillRect(cx + cw, cy, w - (cx + cw), ch); // 右

  // 绘制高亮裁切框边框
  ctx.strokeStyle = "#38bdf8";
  ctx.lineWidth = 2;
  ctx.setLineDash([]);
  ctx.strokeRect(cx, cy, cw, ch);

  // 绘制九宫格辅助线
  ctx.strokeStyle = "rgba(56, 189, 248, 0.3)";
  ctx.lineWidth = 1;
  ctx.setLineDash([4, 4]);
  ctx.beginPath();
  ctx.moveTo(cx + cw / 3, cy);
  ctx.lineTo(cx + cw / 3, cy + ch);
  ctx.moveTo(cx + (cw * 2) / 3, cy);
  ctx.lineTo(cx + (cw * 2) / 3, cy + ch);
  ctx.moveTo(cx, cy + ch / 3);
  ctx.lineTo(cx + cw, cy + ch / 3);
  ctx.moveTo(cx, cy + (ch * 2) / 3);
  ctx.lineTo(cx + cw, cy + (ch * 2) / 3);
  ctx.stroke();
  ctx.setLineDash([]);

  // 绘制四角和四边控制把手
  const handleSize = 8;
  ctx.fillStyle = "#38bdf8";
  const handles = [
    { x: cx, y: cy },
    { x: cx + cw, y: cy },
    { x: cx, y: cy + ch },
    { x: cx + cw, y: cy + ch },
    { x: cx + cw / 2, y: cy },
    { x: cx + cw / 2, y: cy + ch },
    { x: cx, y: cy + ch / 2 },
    { x: cx + cw, y: cy + ch / 2 },
  ];

  for (const p of handles) {
    ctx.fillRect(p.x - handleSize / 2, p.y - handleSize / 2, handleSize, handleSize);
  }
}

// 鼠标交互控制
function getMousePos(e: MouseEvent) {
  const canvas = canvasRef.value;
  if (!canvas) return { x: 0, y: 0 };
  const rect = canvas.getBoundingClientRect();
  return {
    x: ((e.clientX - rect.left) / rect.width) * canvas.width,
    y: ((e.clientY - rect.top) / rect.height) * canvas.height,
  };
}

function detectHandle(x: number, y: number): DragHandle {
  const canvas = canvasRef.value;
  if (!canvas) return "none";
  const scaleX = canvas.width / originalWidth.value;
  const scaleY = canvas.height / originalHeight.value;

  const cx = crop.value.x * scaleX;
  const cy = crop.value.y * scaleY;
  const cw = crop.value.w * scaleX;
  const ch = crop.value.h * scaleY;
  const tol = 12;

  if (Math.abs(x - cx) <= tol && Math.abs(y - cy) <= tol) return "nw";
  if (Math.abs(x - (cx + cw)) <= tol && Math.abs(y - cy) <= tol) return "ne";
  if (Math.abs(x - cx) <= tol && Math.abs(y - (cy + ch)) <= tol) return "sw";
  if (Math.abs(x - (cx + cw)) <= tol && Math.abs(y - (cy + ch)) <= tol) return "se";
  if (Math.abs(y - cy) <= tol && x >= cx && x <= cx + cw) return "n";
  if (Math.abs(y - (cy + ch)) <= tol && x >= cx && x <= cx + cw) return "s";
  if (Math.abs(x - cx) <= tol && y >= cy && y <= cy + ch) return "w";
  if (Math.abs(x - (cx + cw)) <= tol && y >= cy && y <= cy + ch) return "e";
  if (x > cx && x < cx + cw && y > cy && y < cy + ch) return "inside";

  return "none";
}

function onMouseDown(e: MouseEvent) {
  if (!isMediaLoaded.value) return;
  const pos = getMousePos(e);
  activeHandle = detectHandle(pos.x, pos.y);
  if (activeHandle !== "none") {
    startX = pos.x;
    startY = pos.y;
    startCrop = { ...crop.value };
    window.addEventListener("mousemove", onMouseMove);
    window.addEventListener("mouseup", onMouseUp);
  }
}

function onMouseMove(e: MouseEvent) {
  const canvas = canvasRef.value;
  if (!canvas || activeHandle === "none") return;
  const pos = getMousePos(e);
  const scaleX = originalWidth.value / canvas.width;
  const scaleY = originalHeight.value / canvas.height;

  const dx = (pos.x - startX) * scaleX;
  const dy = (pos.y - startY) * scaleY;

  let { x, y, w, h } = startCrop;
  const origW = originalWidth.value;
  const origH = originalHeight.value;
  // 动态采用从设备获取的真实长宽比
  const ratio = deviceWidth.value / deviceHeight.value;

  if (activeHandle === "inside") {
    x = Math.max(0, Math.min(origW - w, Math.round(startCrop.x + dx)));
    y = Math.max(0, Math.min(origH - h, Math.round(startCrop.y + dy)));
  } else {
    if (activeHandle.includes("e")) {
      w = Math.max(80, Math.min(origW - x, Math.round(startCrop.w + dx)));
      if (lockAspect.value) h = Math.round(w / ratio);
    }
    if (activeHandle.includes("s")) {
      h = Math.max(60, Math.min(origH - y, Math.round(startCrop.h + dy)));
      if (lockAspect.value) w = Math.round(h * ratio);
    }
    if (activeHandle.includes("w")) {
      x = Math.max(0, Math.min(startCrop.x + startCrop.w - 80, Math.round(startCrop.x + dx)));
      w = startCrop.w + (startCrop.x - x);
      if (lockAspect.value) h = Math.round(w / ratio);
    }
    if (activeHandle.includes("n")) {
      y = Math.max(0, Math.min(startCrop.y + startCrop.h - 60, Math.round(startCrop.y + dy)));
      h = startCrop.h + (startCrop.y - y);
      if (lockAspect.value) w = Math.round(h * ratio);
    }

    // 边界限制
    if (x + w > origW) w = origW - x;
    if (y + h > origH) h = origH - y;
  }

  crop.value = { x, y, w, h };
  drawCanvas();
}

function onMouseUp() {
  activeHandle = "none";
  window.removeEventListener("mousemove", onMouseMove);
  window.removeEventListener("mouseup", onMouseUp);
}

// 开始推流 (动态携带从设备获取的物理分辨率)
async function startPush(showAlert = true): Promise<boolean> {
  if (!selectedFile.value) {
    if (showAlert) {
      alert("请先选择一个媒体文件！");
    }
    return false;
  }
  errorMessage.value = "";

  try {
    await invoke("start_stream", {
      config: {
        file_path: selectedFile.value,
        fps: fps.value,
        is_loop: isLoop.value,
        crop_x: scaleMode.value === "custom" ? crop.value.x : null,
        crop_y: scaleMode.value === "custom" ? crop.value.y : null,
        crop_w: scaleMode.value === "custom" ? crop.value.w : null,
        crop_h: scaleMode.value === "custom" ? crop.value.h : null,
        scale_mode: scaleMode.value,
        target_width: deviceWidth.value,
        target_height: deviceHeight.value,
        custom_ffmpeg: null,
      },
    });
    isStreaming.value = true;
    return true;
  } catch (e: any) {
    errorMessage.value = "推流启动失败: " + e;
    return false;
  }
}

async function tryAutoStart() {
  if (
    !autoStartPending ||
    autoStartInProgress ||
    !isMediaLoaded.value ||
    isStreaming.value ||
    !deviceOnline.value
  ) {
    return;
  }

  autoStartInProgress = true;
  const started = await startPush(false);
  if (started) {
    autoStartPending = false;
  }
  autoStartInProgress = false;
}

// 停止推流
async function stopPush() {
  try {
    await invoke("stop_stream");
    isStreaming.value = false;
    currentFps.value = 0;
  } catch (e: any) {
    errorMessage.value = "停止推流失败: " + e;
  }
}

watch(lockAspect, (val) => {
  if (val) resetCrop();
  drawCanvas();
});

watch(scaleMode, () => {
  drawCanvas();
});

onMounted(async () => {
  let startMinimized = false;
  try {
    isAutostart.value = await invoke("get_autostart");
  } catch (e) {
    console.error(e);
  }
  try {
    startMinimized = await invoke("get_start_minimized");
  } catch (e) {
    console.error(e);
  }
  try {
    let savedConfig: any = null;
    try {
      savedConfig = await invoke("get_saved_stream_config");
    } catch {
      // 配置恢复失败时继续使用兼容的 localStorage 路径。
    }

    const savedMediaPath =
      localStorage.getItem(selectedMediaStorageKey) || savedConfig?.file_path || null;
    if (savedMediaPath) {
      await loadMedia(savedMediaPath);
      if (savedConfig) {
        if (Number.isFinite(savedConfig.fps) && savedConfig.fps > 0) {
          fps.value = savedConfig.fps;
        }
        if (typeof savedConfig.is_loop === "boolean") {
          isLoop.value = savedConfig.is_loop;
        }
        if (["custom", "cover", "contain", "stretch"].includes(savedConfig.scale_mode)) {
          scaleMode.value = savedConfig.scale_mode;
        }
        if (
          savedConfig.scale_mode === "custom" &&
          [savedConfig.crop_x, savedConfig.crop_y, savedConfig.crop_w, savedConfig.crop_h].every(
            (value) => Number.isFinite(value),
          )
        ) {
          crop.value = {
            x: savedConfig.crop_x,
            y: savedConfig.crop_y,
            w: savedConfig.crop_w,
            h: savedConfig.crop_h,
          };
          drawCanvas();
        }
      }
      autoStartPending = startMinimized && isAutostart.value && isMediaLoaded.value;
    }
  } catch {
    // 恢复失败时保持空白状态，用户仍可重新导入媒体。
  }
  await refreshStatus();
  pollTimer = window.setInterval(refreshStatus, 5000);

  unlistenDrop = await getCurrentWindow().onDragDropEvent((event) => {
    if (event.payload.type === "drop" && event.payload.paths.length > 0) {
      void loadMedia(event.payload.paths[0]);
    }
  });
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
  unlistenDrop?.();
  unlistenDrop = null;
});
</script>

<template>
  <div class="app-shell">
    <!-- 顶部状态导航条 -->
    <header class="navbar">
      <div class="brand">
        <img class="brand-icon" :src="appIcon" alt="Myth.Cool Lite" />
        <div class="brand-text">
          <span class="brand-title">Myth.Cool Lite</span>
          <span class="brand-badge">控制台</span>
        </div>
      </div>

      <div class="nav-status-group">
        <!-- 设备状态 -->
        <div :class="['badge', deviceOnline ? 'badge-green' : 'badge-red']">
          <span class="dot"></span>
          {{ deviceStatusText }}
        </div>

        <!-- 推流状态 -->
        <div :class="['badge', isStreaming ? 'badge-cyan' : 'badge-gray']">
          <span class="dot"></span>
          {{ isStreaming ? `推流中: ${currentFps.toFixed(1)} / ${effectiveFps} FPS` : "待机中" }}
        </div>

        <!-- 开机自启开关 -->
        <label class="autostart-toggle" title="Windows 开机自启">
          <input type="checkbox" v-model="isAutostart" @change="toggleAutostart" />
          <span class="toggle-slider"></span>
          <span class="toggle-label">开机自启</span>
        </label>
      </div>
    </header>

    <!-- 主体区域（严格零滚动一屏流） -->
    <main class="main-content">
      <!-- 左侧：媒体导入与裁切缩放工作台 -->
      <section class="crop-studio">
        <div class="card-header">
          <div class="header-title">
            <span class="header-name">🎬 媒体裁切与画面缩放</span>
            <span v-if="fileName" class="file-tag">{{ fileName }} ({{ originalWidth }}×{{ originalHeight }})</span>
          </div>

          <button class="btn btn-secondary btn-sm" @click="selectMediaFile">
            📂 导入视频/图片/GIF
          </button>
        </div>

        <!-- 画布编辑视口（自适应剩余高度与宽度） -->
        <div class="canvas-wrapper">
          <div v-if="isLoadingMedia" class="overlay-loading">
            <div class="spinner"></div>
            <span>正在分析媒体并提取画面帧...</span>
          </div>

          <canvas
            v-show="isMediaLoaded"
            ref="canvasRef"
            width="560"
            height="420"
            class="crop-canvas"
            @mousedown="onMouseDown"
          ></canvas>

          <div v-if="!isMediaLoaded && !isLoadingMedia" class="empty-placeholder" @click="selectMediaFile">
            <div class="upload-icon">📁</div>
            <p>点击选择或将 <b>MP4 / GIF / 图片</b> 拖拽至此处</p>
            <span class="hint">支持任意长宽比，可在下方工具栏直观裁切并自动适配硬件副屏</span>
          </div>
        </div>

        <!-- 裁切控制条（紧凑双行密集工具栏） -->
        <div class="crop-controls">
          <!-- 上行：比例锁定、快捷按钮、以及数值读数 -->
          <div class="crop-row-top">
            <div class="crop-quick-actions">
              <label class="checkbox-label" title="限制裁切框长宽比与副屏物理分辨率一致">
                <input type="checkbox" v-model="lockAspect" />
                <span>锁定副屏比例</span>
                <span class="control-value">{{ deviceWidth }}×{{ deviceHeight }} · {{ deviceAspectStr }}</span>
              </label>

              <div class="btn-group">
                <button class="btn btn-ghost btn-xs" @click="resetCrop">居中最大</button>
                <button class="btn btn-ghost btn-xs" @click="fitFullCrop">完整画面</button>
              </div>
            </div>

            <div class="crop-info">
              <span class="readout-item"><span class="readout-label">位置</span><b>{{ crop.x }}, {{ crop.y }}</b></span>
              <span class="readout-item"><span class="readout-label">选区</span><b>{{ crop.w }}×{{ crop.h }}</b></span>
              <span class="readout-item"><span class="readout-label">输出</span><b>{{ deviceWidth }}×{{ deviceHeight }}</b></span>
            </div>
          </div>

          <!-- 下行：适配方式分段胶囊 -->
          <div class="mode-selector">
            <span class="mode-title">适配方式</span>
            <div class="mode-options">
              <label :class="['radio-label', { active: scaleMode === 'custom' }]" title="按裁切选区输出">
                <input type="radio" value="custom" v-model="scaleMode" />
                <span class="mode-name">精准裁切</span>
              </label>
              <label :class="['radio-label', { active: scaleMode === 'cover' }]" title="保持比例填满副屏，裁切两边">
                <input type="radio" value="cover" v-model="scaleMode" />
                <span class="mode-name">等比填充</span>
              </label>
              <label :class="['radio-label', { active: scaleMode === 'contain' }]" title="保持比例完整显示，四周留黑">
                <input type="radio" value="contain" v-model="scaleMode" />
                <span class="mode-name">等比完整</span>
              </label>
              <label :class="['radio-label', { active: scaleMode === 'stretch' }]" title="拉伸铺满副屏，忽略原长宽比">
                <input type="radio" value="stretch" v-model="scaleMode" />
                <span class="mode-name">强制拉伸</span>
              </label>
            </div>
          </div>
        </div>
      </section>

      <!-- 右侧：硬件配置与推流控制 -->
      <aside class="control-panel">
        <!-- 核心推流控制按钮 -->
        <div class="action-card">
          <button
            v-if="!isStreaming"
            class="btn btn-primary btn-push"
            :disabled="!isMediaLoaded"
            @click="() => void startPush()"
          >
            🚀 开始推流至副屏
          </button>
          <button v-else class="btn btn-danger btn-push" @click="stopPush">
            ⏹ 停止推流并释放设备
          </button>
          <p class="sub-hint">关闭窗口自动最小化到系统托盘，维持持续推流</p>
        </div>

        <!-- 推流参数卡片 -->
        <div class="param-card">
          <div class="section-title">⚙️ 推流参数设定</div>

          <div class="param-item">
            <div class="param-label-row">
              <label>推帧帧率 (FPS)</label>
              <span class="param-hint">推荐 16 FPS 最稳省 CPU</span>
            </div>
            <div class="fps-selector">
              <button
                v-for="val in [12, 16, 24, 30]"
                :key="val"
                :class="['fps-btn', fps === val ? 'active' : '']"
                @click="fps = val"
              >
                {{ val }}
              </button>
            </div>
          </div>

          <div class="param-item">
            <label class="checkbox-label compact">
              <input type="checkbox" v-model="isLoop" />
              <span>循环播放 (视频 / GIF 无限循环)</span>
            </label>
          </div>
        </div>

        <!-- 硬件与推流实时监控 -->
        <div class="monitor-card">
          <div class="section-title">📊 实时运行指标</div>
          <div class="stat-grid">
            <div class="stat-box">
              <span class="stat-label">目标面板</span>
              <span class="stat-val highlight">{{ deviceWidth }} × {{ deviceHeight }}</span>
            </div>
            <div class="stat-box">
              <span class="stat-label">物理比例</span>
              <span class="stat-val highlight">{{ deviceAspectStr }}</span>
            </div>
            <div class="stat-box">
              <span class="stat-label">色彩通道</span>
              <span class="stat-val">{{ colorChannelText }}</span>
            </div>
            <div class="stat-box">
              <span class="stat-label">推流帧率</span>
              <span class="stat-val" :class="{ highlight: isStreaming }">
                {{ isStreaming ? `${currentFps.toFixed(1)} fps` : "待机中" }}
              </span>
            </div>
          </div>

          <!-- 异常信息警告 -->
          <div v-if="errorMessage" class="error-banner">
            ⚠️ {{ errorMessage }}
          </div>
        </div>

        <!-- 底部作者与外链信息 -->
        <div class="panel-footer">
          <span class="author-credit">ZGMFX01A</span>
          <a class="repository-link" href="#" @click.prevent="openPublicRepository">GitHub 项目主页 ↗</a>
        </div>
      </aside>
    </main>
  </div>
</template>

<style>
/* 全局零边距与严苛视口重置，根除滚动条与白边 */
html,
body,
#app {
  margin: 0 !important;
  padding: 0 !important;
  width: 100vw !important;
  height: 100vh !important;
  overflow: hidden !important;
  background-color: #0b0f19 !important;
  border: none !important;
  outline: none !important;
  box-sizing: border-box !important;
}

*,
*::before,
*::after {
  box-sizing: border-box !important;
}
</style>

<style scoped>
/* 主题调色盘：深邃电竞暗黑风，冰川蓝点缀 */
.app-shell {
  display: flex;
  flex-direction: column;
  width: 100vw;
  height: 100vh;
  margin: 0;
  padding: 0;
  background-color: #0b0f19;
  color: #f1f5f9;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", sans-serif;
  user-select: none;
  overflow: hidden;
}

/* 顶部导航条：收敛精致高度与纯净色调 */
.navbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  height: 46px;
  flex: 0 0 46px;
  padding: 0 16px;
  background-color: #0f172a;
  border-bottom: 1px solid rgba(255, 255, 255, 0.08);
}

.brand {
  display: flex;
  align-items: center;
  gap: 8px;
}

.brand-icon {
  width: 22px;
  height: 22px;
  flex: 0 0 22px;
  object-fit: contain;
  border-radius: 5px;
}

.brand-text {
  display: flex;
  align-items: center;
  gap: 6px;
}

.brand-title {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
  letter-spacing: -0.2px;
  color: #f1f5f9;
  line-height: 1;
}

.brand-badge {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 4px;
  background: rgba(56, 189, 248, 0.12);
  color: #38bdf8;
  font-weight: 500;
  border: 1px solid rgba(56, 189, 248, 0.22);
  line-height: 1.2;
}

.nav-status-group {
  display: flex;
  align-items: center;
  gap: 12px;
}

/* 状态 Badge */
.badge {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  border-radius: 9999px;
  font-size: 12px;
  font-weight: 500;
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
}

.badge-green {
  background-color: rgba(34, 197, 94, 0.15);
  color: #4ade80;
  border: 1px solid rgba(34, 197, 94, 0.3);
}
.badge-green .dot {
  background-color: #22c55e;
  box-shadow: 0 0 6px #22c55e;
}

.badge-red {
  background-color: rgba(239, 68, 68, 0.15);
  color: #f87171;
  border: 1px solid rgba(239, 68, 68, 0.3);
}
.badge-red .dot {
  background-color: #ef4444;
}

.badge-cyan {
  background-color: rgba(56, 189, 248, 0.15);
  color: #38bdf8;
  border: 1px solid rgba(56, 189, 248, 0.3);
}
.badge-cyan .dot {
  background-color: #38bdf8;
  box-shadow: 0 0 6px #38bdf8;
}

.badge-gray {
  background-color: #1f2937;
  color: #94a3b8;
  border: 1px solid rgba(255, 255, 255, 0.05);
}
.badge-gray .dot {
  background-color: #64748b;
}

/* 自启动开关 */
.autostart-toggle {
  display: flex;
  align-items: center;
  gap: 6px;
  cursor: pointer;
  font-size: 12px;
  color: #cbd5e1;
}

.autostart-toggle input {
  display: none;
}

.toggle-slider {
  width: 32px;
  height: 18px;
  background-color: #334155;
  border-radius: 9999px;
  position: relative;
  transition: 0.2s;
}

.toggle-slider::after {
  content: "";
  position: absolute;
  top: 2px;
  left: 2px;
  width: 14px;
  height: 14px;
  background-color: #fff;
  border-radius: 50%;
  transition: 0.2s;
}

.autostart-toggle input:checked + .toggle-slider {
  background-color: #0284c7;
}

.autostart-toggle input:checked + .toggle-slider::after {
  transform: translateX(14px);
}

/* 主容器布局：严格零滚动一屏流 */
.main-content {
  display: grid;
  grid-template-columns: 1fr 310px;
  gap: 12px;
  padding: 10px 14px;
  flex: 1;
  min-height: 0;
  min-width: 0;
  overflow: hidden;
}

/* 裁切工作台 */
.crop-studio {
  display: flex;
  flex-direction: column;
  background-color: #111827;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  padding: 10px 12px;
  gap: 8px;
  min-height: 0;
  height: 100%;
  overflow: hidden;
}

.card-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  flex: 0 0 auto;
  min-height: 28px;
}

.header-title {
  font-size: 14px;
  font-weight: 600;
  color: #e2e8f0;
  display: flex;
  align-items: center;
  gap: 8px;
}

.file-tag {
  font-size: 11px;
  color: #38bdf8;
  background: rgba(56, 189, 248, 0.1);
  border: 1px solid rgba(56, 189, 248, 0.2);
  padding: 2px 7px;
  border-radius: 4px;
}

.canvas-wrapper {
  position: relative;
  flex: 1;
  min-height: 0;
  background-color: #030712;
  border: 1px dashed rgba(255, 255, 255, 0.15);
  border-radius: 8px;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}

.crop-canvas {
  cursor: crosshair;
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
  box-shadow: 0 4px 20px rgba(0, 0, 0, 0.5);
}

.empty-placeholder {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  padding: 24px;
  color: #94a3b8;
  text-align: center;
  gap: 8px;
}

.upload-icon {
  font-size: 40px;
  opacity: 0.8;
}

.empty-placeholder b {
  color: #38bdf8;
}

.hint {
  font-size: 11px;
  color: #64748b;
}

.overlay-loading {
  position: absolute;
  inset: 0;
  background-color: rgba(11, 15, 25, 0.85);
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  z-index: 10;
}

.spinner {
  width: 28px;
  height: 28px;
  border: 3px solid #38bdf8;
  border-top-color: transparent;
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}

/* 裁切控制栏：紧凑密集双行工具栏 */
.crop-controls {
  display: flex;
  flex-direction: column;
  gap: 6px;
  background-color: #1a2234;
  padding: 8px 10px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.05);
  flex: 0 0 auto;
}

.crop-row-top {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 10px;
}

.crop-quick-actions {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: nowrap;
}

.checkbox-label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  user-select: none;
}

.checkbox-label.compact {
  font-size: 12px;
}

.control-value {
  color: #7dd3fc;
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  background: rgba(56, 189, 248, 0.12);
  border: 1px solid rgba(56, 189, 248, 0.2);
  border-radius: 999px;
  padding: 1px 6px;
}

.btn-group {
  display: flex;
  gap: 6px;
  flex: 0 0 auto;
}

.crop-info {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 7px;
  border: 1px solid rgba(51, 65, 85, 0.7);
  border-radius: 5px;
  background: rgba(15, 23, 42, 0.65);
  font-size: 10.5px;
  color: #64748b;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  white-space: nowrap;
}

.readout-item {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.readout-label {
  color: #64748b;
}

.readout-item b {
  color: #cbd5e1;
  font-weight: 500;
  font-variant-numeric: tabular-nums;
}

.mode-selector {
  display: grid;
  grid-template-columns: 56px 1fr;
  align-items: center;
  gap: 8px;
}

.mode-title {
  color: #94a3b8;
  font-size: 12px;
  white-space: nowrap;
}

.mode-options {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 6px;
}

.radio-label {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  height: 28px;
  padding: 0 6px;
  border: 1px solid #334155;
  border-radius: 6px;
  background: #111827;
  color: #cbd5e1;
  font-size: 11.5px;
  cursor: pointer;
  transition: border-color 0.15s, background-color 0.15s, color 0.15s;
  white-space: nowrap;
}

.radio-label:hover {
  border-color: #64748b;
}

.radio-label.active {
  border-color: #38bdf8;
  background: rgba(14, 116, 144, 0.28);
  color: #f8fafc;
  box-shadow: inset 0 0 0 1px rgba(56, 189, 248, 0.15);
}

.radio-label input {
  margin: 0;
  accent-color: #38bdf8;
}

.mode-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 右侧控制面板 */
.control-panel {
  display: flex;
  flex-direction: column;
  gap: 8px;
  height: 100%;
  min-height: 0;
  overflow: hidden;
}

.action-card {
  background: linear-gradient(180deg, #1e293b 0%, #0f172a 100%);
  border: 1px solid rgba(56, 189, 248, 0.25);
  border-radius: 10px;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  flex: 0 0 auto;
}

.btn-push {
  width: 100%;
  height: 38px;
  font-size: 14px;
  font-weight: 600;
  border-radius: 7px;
}

.sub-hint {
  font-size: 11px;
  color: #64748b;
  margin: 0;
  text-align: center;
  line-height: 1.3;
}

.param-card,
.monitor-card {
  background-color: #111827;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 7px;
  flex: 0 0 auto;
}

.section-title {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
  color: #e2e8f0;
}

.param-item {
  display: flex;
  flex-direction: column;
  gap: 5px;
}

.param-label-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 11.5px;
  color: #94a3b8;
}

.param-hint {
  font-size: 10.5px;
  color: #64748b;
}

.fps-selector {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 6px;
}

.fps-btn {
  height: 26px;
  background-color: #1e293b;
  border: 1px solid #334155;
  color: #e2e8f0;
  border-radius: 5px;
  cursor: pointer;
  font-size: 12px;
  font-weight: 500;
  transition: 0.15s;
  display: flex;
  align-items: center;
  justify-content: center;
}

.fps-btn.active {
  background-color: #0284c7;
  border-color: #38bdf8;
  color: #fff;
  box-shadow: 0 0 6px rgba(56, 189, 248, 0.3);
}

.stat-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 6px;
}

.stat-box {
  background-color: #182234;
  border-radius: 6px;
  padding: 5px 8px;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.stat-label {
  font-size: 10px;
  color: #94a3b8;
}

.stat-val {
  font-size: 13px;
  font-weight: 600;
  color: #f8fafc;
  font-variant-numeric: tabular-nums;
}

.stat-val.highlight {
  color: #38bdf8;
}

.error-banner {
  background-color: rgba(239, 68, 68, 0.1);
  border: 1px solid rgba(239, 68, 68, 0.3);
  color: #fca5a5;
  font-size: 11px;
  padding: 6px 8px;
  border-radius: 5px;
  line-height: 1.3;
}

.panel-footer {
  margin-top: auto;
  display: flex;
  justify-content: space-between;
  align-items: center;
  min-height: 22px;
  padding: 0 2px;
  flex: 0 0 auto;
}

.author-credit {
  color: #475569;
  font-size: 11px;
}

.repository-link {
  display: inline-flex;
  align-items: center;
  min-height: 22px;
  padding: 0 8px;
  border: 1px solid rgba(56, 189, 248, 0.4);
  border-radius: 5px;
  background: rgba(14, 116, 144, 0.15);
  color: #7dd3fc;
  font-size: 11px;
  font-weight: 500;
  text-decoration: none;
  transition: color 0.15s ease, background-color 0.15s ease, border-color 0.15s ease;
}

.repository-link:hover {
  border-color: #7dd3fc;
  background: rgba(14, 165, 233, 0.28);
  color: #f0f9ff;
}

/* 按钮通用规范 */
.btn {
  border: none;
  border-radius: 6px;
  font-weight: 500;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 5px;
  transition: all 0.15s;
}

.btn:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.btn-primary {
  background: linear-gradient(135deg, #0284c7 0%, #2563eb 100%);
  color: #fff;
  box-shadow: 0 2px 8px rgba(2, 132, 199, 0.3);
}

.btn-primary:hover:not(:disabled) {
  background: linear-gradient(135deg, #0369a1 0%, #1d4ed8 100%);
}

.btn-danger {
  background: linear-gradient(135deg, #dc2626 0%, #b91c1c 100%);
  color: #fff;
  box-shadow: 0 2px 8px rgba(220, 38, 38, 0.3);
}

.btn-danger:hover {
  background: linear-gradient(135deg, #b91c1c 0%, #991b1b 100%);
}

.btn-secondary {
  background-color: #1e293b;
  border: 1px solid #334155;
  color: #e2e8f0;
}

.btn-secondary:hover {
  background-color: #334155;
}

.btn-ghost {
  background-color: transparent;
  color: #94a3b8;
  border: 1px solid #334155;
}

.btn-ghost:hover {
  color: #f8fafc;
  background-color: #1e293b;
}

.btn-sm {
  padding: 4px 10px;
  font-size: 12px;
}

.btn-xs {
  padding: 2px 7px;
  font-size: 11px;
}
</style>
