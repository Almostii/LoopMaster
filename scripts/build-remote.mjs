#!/usr/bin/env node
// LoopMaster 统一构建入口（远程前端部分）。
//
// 干净检出的构建顺序（方案 2 §3 硬约束）：
//   1. `node scripts/build-remote.mjs` —— 安装依赖并构建 frontend-remote/dist
//   2. `cargo build/test`（根 workspace 或 frontend/src-tauri）
// rust-embed 在编译期内联 frontend-remote/dist；dist 不进 Git，Cargo 不能
// 隐式依赖开发机残留产物（app-service/build.rs 会在缺失时生成占位页，
// 但正式分发必须先跑本脚本）。

import { spawnSync } from "node:child_process";
import { existsSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const remoteDir = join(repoRoot, "frontend-remote");
const distDir = join(remoteDir, "dist");

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (result.status !== 0) {
    console.error(`[build-remote] 命令失败: ${command} ${args.join(" ")}`);
    process.exit(result.status ?? 1);
  }
}

console.log("[build-remote] 1/2 安装 frontend-remote 依赖（npm ci / npm install）...");
const lockfile = join(remoteDir, "package-lock.json");
if (existsSync(lockfile)) {
  run("npm", ["ci"], remoteDir);
} else {
  // 首次构建尚无 lockfile：install 生成后提交，后续固定 npm ci。
  run("npm", ["install"], remoteDir);
  console.log("[build-remote] 注意：已生成 package-lock.json，请一并提交以锁定依赖。");
}

console.log("[build-remote] 2/2 构建 frontend-remote/dist ...");
run("npm", ["run", "build"], remoteDir);

if (!existsSync(join(distDir, "index.html"))) {
  console.error("[build-remote] 构建完成但 dist/index.html 缺失，请检查 vite 配置。");
  process.exit(1);
}
console.log(`[build-remote] 完成：${distDir}`);
console.log("[build-remote] 下一步：cargo build / cargo test（dist 将被 rust-embed 内联）。");
