# LoopMaster 前端

正式前端技术栈为 `Tauri 2 + React + TypeScript`。本目录是前端工程，独立于根 Rust workspace，通过 `frontend/src-tauri` 路径依赖 `app-service` 作为唯一业务服务边界。

## 当前状态（2026-08-25，分支 `codex/feature-tauri-init`）

已完成第一阶段壳层初始化：

- Tauri 2 官方 React + TypeScript 模板已在本目录生成；
- 最小窗口可启动，标题为「LoopMaster 音频路由」；
- `frontend/src-tauri` 作为独立 Tauri crate（声明独立 `[workspace]`），通过路径依赖使用 `app-service`；
- 提供 `list_devices` command 作为 src-tauri 访问 app-service 的最小验证接口；
- React 最小页面调用 `list_devices` 展示设备枚举，用于验证命令链路；
- `package-lock.json` 与 `src-tauri/Cargo.lock` 已提交，`node_modules`、`dist`、`target`、`gen/schemas` 均不入库。

## 常用命令

```powershell
cd frontend
npm install
npm run tauri dev      # 启动 Tauri 开发窗口
npm run build          # 仅前端构建
npx tauri build --no-bundle  # 构建应用二进制（不含安装器）
```

## 约定

- React 只负责界面、交互和展示状态；
- Tauri command/event 是前端与 Rust 应用服务之间的边界；
- 前端不得直接调用 WASAPI、读取音频 FIFO 或持有实时线程对象；
- 稳定的 endpoint ID、Route Profile（Source、Output Channel、External Output、Monitor）DTO 和错误分类以 Tauri/app-service 适配契约为准；内部 source/bus/sink/send 仅是 Rust 实现模型；
- 前端依赖和构建产物使用 Node/Tauri 工具链管理，不加入根 Rust workspace 成员；
- `frontend/src-tauri` 声明独立 `[workspace]`，脱离根 workspace；如需加入根 workspace 必须先评估 workspace 边界与构建时间。

## 后续

下一阶段（阶段 B）实现 Tauri command/event 适配层闭环：`list_devices`、`list_audio_processes`、`get_route_snapshot`、引擎启停/重连、路由编辑，以及状态/统计/设备丢失恢复事件，再进入主路由工作区 MVP。
