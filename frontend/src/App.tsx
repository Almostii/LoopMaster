import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

/** 设备概要 DTO，与 src-tauri 的 DeviceBrief 对应。 */
interface DeviceBrief {
  id: string;
  name: string;
  flow: "capture" | "render";
}

function App() {
  const [devices, setDevices] = useState<DeviceBrief[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function refreshDevices() {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<DeviceBrief[]>("list_devices");
      setDevices(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    refreshDevices();
  }, []);

  return (
    <main className="container">
      <h1>LoopMaster 音频路由</h1>
      <p>前端壳层已启动。下方设备列表来自 Rust 应用服务（app-service）枚举。</p>

      <div className="row">
        <button onClick={refreshDevices} disabled={loading}>
          {loading ? "正在枚举…" : "刷新设备"}
        </button>
      </div>

      {error && (
        <p className="error">枚举失败：{error}</p>
      )}

      {!error && devices.length === 0 && !loading && (
        <p>未发现设备。</p>
      )}

      {devices.length > 0 && (
        <ul className="device-list">
          {devices.map((d) => (
            <li key={d.id}>
              <span className="device-name">{d.name}</span>
              <span className={`flow ${d.flow}`}>
                {d.flow === "capture" ? "输入" : "输出"}
              </span>
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}

export default App;
