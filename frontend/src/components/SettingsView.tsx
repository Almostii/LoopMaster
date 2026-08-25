import { useEffect } from "react";
import type { AppSettings } from "../types";

/**
 * 基础设置页。
 * 数据来自 useLoopMaster 的 settings state，变更通过 onChange 上报并持久化。
 */
export default function SettingsView({
  settings,
  onChange,
}: {
  settings: AppSettings;
  onChange: (patch: Partial<AppSettings>) => void;
}) {
  // 主题应用到根元素 data-theme，供 CSS 变量切换（后续实现深色样式）。
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", settings.theme);
  }, [settings.theme]);

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
              value={settings.theme}
              onChange={(e) => onChange({ theme: e.target.value })}
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
                checked={settings.start_on_boot}
                onChange={(e) => onChange({ start_on_boot: e.target.checked })}
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
                checked={settings.launch_hidden}
                onChange={(e) => onChange({ launch_hidden: e.target.checked })}
              />
              <span className="slider-round" />
            </label>
          </div>
        </div>
      </div>
    </div>
  );
}
