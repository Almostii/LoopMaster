# LoopMaster 前端

正式前端技术栈为 `Tauri 2 + React + TypeScript`。本目录是前端工程，独立于根 Rust workspace，通过 `frontend/src-tauri` 路径依赖 `app-service` 作为唯一业务服务边界。

## 当前状态（2026-08-25，阶段 A + 阶段 B + 阶段 C 完成）

### 阶段 A：壳层初始化（已合并）

- Tauri 2 官方 React + TypeScript 模板；
- 最小窗口可启动，标题为「LoopMaster 音频路由」；
- `frontend/src-tauri` 作为独立 Tauri crate（声明独立 `[workspace]`），通过路径依赖使用 `app-service`。

### 阶段 B：command/event 适配层闭环（已合并）

Tauri command/event 适配层已建立，React 通过命令访问 app-service、通过事件订阅引擎状态：

- 只读命令：`list_devices`、`list_audio_processes`、`get_route_snapshot`、`get_engine_state`、`get_engine_stats`；
- 写命令：`start_engine`、`stop_engine`、`request_reconnect`、`apply_route_edit`（拓扑变化会返回「需要重启」结构化错误）；
- 事件：`engine-state-changed`、`engine-stats-changed`、`device-lost`、`device-restored`；
- 引擎在首次 `start_engine` 时惰性创建（空图无法初始化，需至少一个 source 和一个 sink）；
- React 界面：引擎状态徽标、引擎控制、音源添加、输出目标添加、路由连线、设备列表、状态/统计与错误/提示展示（中文界面）。

### 阶段 C：主路由工作区 MVP（已合并）

Loopback 风格三列拓扑路由画布，UI 严格参考 `Library/UI-Demo-HTML/`：

- 三列布局：Sources（音频来源）→ Output Channels（输出通道）→ External Outputs（外部输出/监听）；
- Loopback 视觉：主色青绿 `#29b6a2`、浅灰背景、白卡片圆角阴影、On/Off 红青胶囊开关、双行电平表、SVG 三次贝塞尔连线（On=青绿、Off=暗黑）；
- 交互：拖拽连线、点击连线选中删除、添加音源/输出通道/外部输出、开关切换、增益/静音、引擎启停；
- 前端模块化：`api.ts`、`types.ts`、`lib.ts`、`useLoopMaster.ts`、`components/`；
- 后端适配层新增：节点重命名（`set_source_name`/`set_output_channel_name`/`set_external_output_name`）、send 通道映射（`set_send_channel_map`）；
- 严格遵守产品边界：未实现虚拟设备/Pass-Thru/新建虚拟设备等禁用概念；Output Channel 为独立产品概念，不宣称为系统设备。

## 常用命令

```powershell
cd frontend
npm install
npm run tauri dev      # 启动 Tauri 开发窗口
npm run build          # 仅前端构建（tsc + vite）
npx tauri build --no-bundle  # 构建应用二进制（不含安装器）
cd src-tauri && cargo test   # 运行适配层单元测试
```

## 约定

- React 只负责界面、交互和展示状态；
- Tauri command/event 是前端与 Rust 应用服务之间的边界；
- 前端不得直接调用 WASAPI、读取音频 FIFO 或持有实时线程对象；
- 稳定的 endpoint ID、Route Profile（Source、Output Channel、External Output、Monitor）DTO 和错误分类以 Tauri/app-service 适配契约为准；内部 source/bus/sink/send 仅是 Rust 实现模型；
- 前端依赖和构建产物使用 Node/Tauri 工具链管理，不加入根 Rust workspace 成员；
- `frontend/src-tauri` 声明独立 `[workspace]`，脱离根 workspace；如需加入根 workspace 必须先评估 workspace 边界与构建时间。

## 后续

阶段 C 主路由工作区 MVP 已实现并合并。下一阶段（阶段 D）：真实设备门禁（VB-CABLE 闭环、物理 USB 声卡、44.1/48 kHz、16-bit PCM、单声道/双声道、Device/Process Loopback、拔插恢复、30 分钟与 2 小时连续运行）。可选增强：节点重命名 UI、channel map 编辑 UI、逐通道电平表、预设管理。
