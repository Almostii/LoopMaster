import { useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { AppSettings } from "../types";
import { getAppVersion } from "../api";

/** GitHub 公开仓库更新源。 */
const UPDATE_REPO = "Almostii/LoopMaster";
const UPDATE_API = `https://api.github.com/repos/${UPDATE_REPO}/releases/latest`;
const UPDATE_PAGE = `https://github.com/${UPDATE_REPO}/releases/latest`;

interface UpdateInfo {
  current: string;
  latest: string;
  hasUpdate: boolean;
  notes?: string;
}

/** 语义化版本号比较：v0.1.0 / 0.1.0 均可；返回 a > b。 */
function isNewer(a: string, b: string): boolean {
  const pa = a.replace(/^v/i, "").split(".").map(Number);
  const pb = b.replace(/^v/i, "").split(".").map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const na = pa[i] ?? 0;
    const nb = pb[i] ?? 0;
    if (na !== nb) return na > nb;
  }
  return false;
}

/**
 * 基础设置页。
 * 数据来自 useLoopMaster 的 settings state，变更通过 onChange 上报并持久化。
 * 含"更新"区：检查 GitHub 公开仓库最新版本，可跳转下载。
 */
export default function SettingsView({
  settings,
  onChange,
}: {
  settings: AppSettings;
  onChange: (patch: Partial<AppSettings>) => void;
}) {
  const [checking, setChecking] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 主题应用到根元素 data-theme，供 CSS 变量切换（后续实现深色样式）。
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", settings.theme);
  }, [settings.theme]);

  async function handleCheckUpdate() {
    setChecking(true);
    setError(null);
    setUpdate(null);
    try {
      const current = await getAppVersion();
      const res = await fetch(UPDATE_API, { headers: { Accept: "application/vnd.github+json" } });
      if (!res.ok) {
        throw new Error(`GitHub 响应 ${res.status}`);
      }
      const data = await res.json();
      const latest: string = data.tag_name ?? "";
      setUpdate({
        current,
        latest,
        hasUpdate: isNewer(latest, current),
        notes: typeof data.body === "string" ? data.body : undefined,
      });
    } catch (e) {
      setError(`检查更新失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setChecking(false);
    }
  }

  function handleDownload() {
    void openUrl(UPDATE_PAGE);
  }

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

      <div className="settings-section">
        <h3 className="settings-section-title">更新</h3>

        <div className="setting-row">
          <div className="setting-info">
            <div className="setting-label">检查更新</div>
            <div className="setting-desc">
              当前版本 {update?.current ?? "…"}，检查 {UPDATE_REPO} 是否有新版本
            </div>
          </div>
          <div className="setting-control">
            <button
              type="button"
              className="btn-secondary"
              disabled={checking}
              onClick={handleCheckUpdate}
            >
              {checking ? "检查中…" : "检查更新"}
            </button>
          </div>
        </div>

        {update && (
          <div className="setting-update-result">
            {update.hasUpdate ? (
              <>
                <div className="setting-update-new">发现新版本 {update.latest}（当前 {update.current}）</div>
                <button
                  type="button"
                  className="btn-secondary btn-download"
                  onClick={handleDownload}
                >
                  去下载
                </button>
              </>
            ) : (
              <div className="setting-update-ok">已是最新版本（{update.current}）</div>
            )}
          </div>
        )}

        {error && <p className="setting-update-error">{error}</p>}
      </div>
    </div>
  );
}
