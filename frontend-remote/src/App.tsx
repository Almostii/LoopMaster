export default function App() {
  // 占位页：正式触控调音台 UI 在子任务 3 实现（方案 3）。
  return (
    <main className="remote-shell">
      <h1>LoopMaster Remote</h1>
      <p>内嵌 Web 控制台占位页。</p>
      <p className="remote-hint">
        触控调音台界面将在 Phase 2 子任务 3 交付；当前页面用于验证 HTTPS
        分发与 rust-embed 打包链路。
      </p>
    </main>
  );
}
