import { useState } from "react";

/**
 * 基础设置页 (占位骨架)。
 * 后续在此扩展具体设置项：音频引擎、设备默认、主题、快捷键等。
 */
export default function SettingsView() {
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [theme, setTheme] = useState<"light" | "dark">("light");
  const [startOnBoot, setStartOnBoot] = useState(false);
  const [launchHidden, setLaunchHidden] = useState(false);

  return (
    <div className="settings-view">
      <div className="settings-head">
        <h2 className="settings-title">设置</h2>
        <p className="settings-subtitle">配置 LoopMaster 的基础行为</p>
      </div>

      <div className="settings-section">
        <h3 className="settings-section-title">通用</h3>

        <div className="setting-row">
          <div className="setting-info">
            <div className="setting-label">外观主题</div>
            <div className="setting-desc">切换应用配色方案</div>
          </div>
          <div className="setting-control">
            <select
              className="setting-select"
              value={theme}
              onChange={(e) => setTheme(e.target.value as "light" | "dark")}
            >
              <option value="light">浅色</option>
              <option value="dark">深色</option>
            </select>
          </div>
        </div>

        <div className="setting-row">
          <div className="setting-info">
            <div className="setting-label">开机自启动</div>
            <div className="setting-desc">系统启动时自动运行 LoopMaster</div>
          </div>
          <div className="setting-control">
            <label className="switch" title="开机自启动">
              <input
                type="checkbox"
                checked={startOnBoot}
                onChange={(e) => setStartOnBoot(e.target.checked)}
              />
              <span className="slider-round" />
            </label>
          </div>
        </div>

        <div className="setting-row">
          <div className="setting-info">
            <div className="setting-label">启动时隐藏主窗口</div>
            <div className="setting-desc">仅驻留系统托盘，不弹出主窗口</div>
          </div>
          <div className="setting-control">
            <label className="switch" title="启动时隐藏主窗口">
              <input
                type="checkbox"
                checked={launchHidden}
                onChange={(e) => setLaunchHidden(e.target.checked)}
              />
              <span className="slider-round" />
            </label>
          </div>
        </div>
      </div>

      <div className="settings-section">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => setShowAdvanced((v) => !v)}
        >
          {showAdvanced ? "收起高级选项" : "高级选项"}
        </button>
        {showAdvanced && (
          <p className="setting-hint">
            高级选项尚未实现，待确认具体需求后补充。
          </p>
        )}
      </div>
    </div>
  );
}
