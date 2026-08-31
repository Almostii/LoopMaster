import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 远程控制台构建配置：产物输出到 dist/（由 rust-embed 内联进桌面二进制）。
// dist 不进 Git；仓库根目录 `node scripts/build-remote.mjs` 是统一构建入口。
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    sourcemap: false,
  },
  server: {
    // 本地开发时直连桌面端 HTTPS 服务需另行配置代理；当前占位页无网络依赖。
    port: 5174,
  },
});
