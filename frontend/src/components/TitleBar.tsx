import type { EngineStateBrief } from "../types";

export default function TitleBar({
  engineState,
  onToggleEngine,
}: {
  engineState: EngineStateBrief;
  onToggleEngine: (running: boolean) => void;
}) {
  const badgeClass = `engine-status-badge ${
    engineState.running ? "active" : ""
  }`;
  // 后端 as_str() 返回首字母大写（"Stopped" 等），前端匹配需大小写不敏感。
  const stateKey = engineState.state.toLowerCase();
  const dotClass = `status-dot status-${stateKey}`;
  const stateLabel =
    {
      stopped: "已停止",
      running: "运行中",
      degraded: "降级",
      reconnecting: "重连中",
      failed: "失败",
    }[stateKey] ?? engineState.state;

  return (
    <header className="titlebar">
      <div className="titlebar-left">
        <div className="app-brand">
          <div className="brand-icon">
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <polyline points="18 15 12 9 6 15" />
              <polyline points="18 9 12 3 6 9" />
            </svg>
          </div>
          <span>LoopMaster</span>
          <span className="brand-subtitle">音频路由</span>
        </div>
      </div>

      <div className="titlebar-center">
        <div className={badgeClass} id="engine-status-badge">
          <span className={dotClass} />
          <span>音频引擎 · {stateLabel}</span>
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
      </div>
    </header>
  );
}
