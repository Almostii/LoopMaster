import { getCurrentWindow } from "@tauri-apps/api/window";
import type { EngineStateBrief } from "../types";

export default function TitleBar({
  engineState,
  onToggleEngine,
}: {
  engineState: EngineStateBrief;
  onToggleEngine: (running: boolean) => void;
}) {
  // 后端 as_str() 返回首字母大写（"Stopped" 等），前端匹配需大小写不敏感。
  const stateKey = engineState.state.toLowerCase();
  const badgeClass = `engine-status-badge ${engineState.running ? "active" : ""}`;
  const dotClass = `status-dot status-${stateKey}`;
  const stateLabel =
    {
      stopped: "已停止",
      running: "运行中",
      degraded: "降级",
      reconnecting: "重连中",
      failed: "失败",
    }[stateKey] ?? engineState.state;

  const appWindow = getCurrentWindow();

  // 拖拽区域的事件处理：Tauri 2 使用 data-tauri-drag-region 属性实现拖动，
  // 需要 core:window:allow-start-dragging 权限。mousedown 命中可交互元素
  // (按钮/开关/输入)时停止事件冒泡，避免误触发拖动。
  function handleDragMouseDown(e: React.MouseEvent) {
    const target = e.target as HTMLElement;
    const interactive = target.closest("button, input, label.switch, a");
    if (interactive) {
      e.stopPropagation();
    }
  }

  async function handleMinimize() {
    try {
      await appWindow.minimize();
    } catch (err) {
      console.error("最小化窗口失败:", err);
    }
  }

  async function handleToggleMaximize() {
    try {
      const isMax = await appWindow.isMaximized();
      if (isMax) {
        await appWindow.unmaximize();
      } else {
        await appWindow.toggleMaximize();
      }
    } catch (err) {
      console.error("切换最大化失败:", err);
    }
  }

  async function handleClose() {
    // 后端 on_window_event 已拦截 CloseRequested 并 hide，前端调用 close 即可
    try {
      await appWindow.close();
    } catch (err) {
      console.error("关闭窗口失败:", err);
    }
  }

  return (
    <header
      className="titlebar"
      data-tauri-drag-region
      onMouseDown={handleDragMouseDown}
    >
      <div className="titlebar-left">
        <div className="brand-icon">
          <img src="/loopmaster-logo.svg" alt="" />
        </div>
        <span className="titlebar-app-name">LoopMaster</span>
      </div>

      <div className="titlebar-center">
        <div className={badgeClass} id="engine-status-badge">
          <span className={dotClass} />
          <span>音频路由 · {stateLabel}</span>
        </div>
        {engineState.last_error && (
          <span className="titlebar-error" title={engineState.last_error}>
            最近错误：{engineState.last_error}
          </span>
        )}
      </div>

      <div className="titlebar-right">
        <label className="switch" title="总引擎开关">
          <input
            type="checkbox"
            checked={engineState.running}
            onChange={(e) => onToggleEngine(e.target.checked)}
          />
          <span className="slider-round" />
        </label>
        <div className="window-controls">
          <button
            type="button"
            className="win-btn win-btn-min"
            onClick={handleMinimize}
            aria-label="最小化"
            title="最小化"
          >
            <svg width="10" height="10" viewBox="0 0 10 10">
              <line x1="0" y1="5" x2="10" y2="5" stroke="currentColor" strokeWidth="1" />
            </svg>
          </button>
          <button
            type="button"
            className="win-btn win-btn-max"
            onClick={handleToggleMaximize}
            aria-label="最大化"
            title="最大化"
          >
            <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
              <rect x="0.5" y="0.5" width="9" height="9" />
            </svg>
          </button>
          <button
            type="button"
            className="win-btn win-btn-close"
            onClick={handleClose}
            aria-label="关闭"
            title="关闭（隐藏到托盘，从托盘菜单退出）"
          >
            <svg width="10" height="10" viewBox="0 0 10 10">
              <line x1="0" y1="0" x2="10" y2="10" stroke="currentColor" strokeWidth="1" />
              <line x1="10" y1="0" x2="0" y2="10" stroke="currentColor" strokeWidth="1" />
            </svg>
          </button>
        </div>
      </div>
    </header>
  );
}
