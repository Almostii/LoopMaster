# LoopMaster 前端

这里是新前端的预留目录，目标技术栈为 `Tauri 2 + React + TypeScript`。

当前只完成目录和架构边界准备，尚未生成 Tauri 工程，也没有可运行的 React 页面。后续初始化 Tauri 时，应在本目录内生成前端工程，不要把前端代码重新放回 Rust workspace 的 `app/` 目录。

## 约定

- React 只负责界面、交互和展示状态；
- Tauri command/event 是前端与 Rust 应用服务之间的边界；
- 前端不得直接调用 WASAPI、读取音频 FIFO 或持有实时线程对象；
- 稳定的 endpoint ID、source/sink/send schema 和错误分类以 `app-service` 公开契约为准；
- 前端依赖和构建产物必须使用 Node/Tauri 工具链管理，不加入 Rust workspace 成员；
- `node_modules/`、前端构建产物和本地配置不得提交。

## 初始化顺序

1. 确认 Node.js、Rust、Tauri CLI 和 Windows WebView2 环境；
2. 使用 Tauri 2 官方模板在本目录生成 React + TypeScript 工程；
3. 先实现设备/进程枚举和只读状态展示，再接入路由编辑命令；
4. 以真实服务事件驱动 UI，不复制音频引擎状态机；
5. 通过 Windows 打包和最小端到端路由验收后，再删除本文件中的准备状态说明。
