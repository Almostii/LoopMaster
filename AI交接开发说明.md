# LoopMaster AI 开发交接说明

更新时间：2026-08-25

本文是交给下一位 AI coding agent 的执行入口。开始工作前必须完整阅读本文，并继续阅读 Doc/Main/ 中的当前规范、Doc/Log/ 中最近的开发记录。本文描述的是当前代码事实；较早日志和主文档中的旧状态描述不能覆盖本文或最新代码。2026-08-25 起前端已决定从 Slint 迁移到 Tauri 2 + React。

## 1. 当前工程状态

当前工作分支：`codex/migrate-tauri-react-frontend`

本分支基于 `codex/feature-bus-graph` 的提交 `d275910`，不是可直接发布的 `main` 基线。迁移准备完成后，必须先修复基线兼容错误并通过门禁，再由审查者合并到 `main`。

相关最近提交：

    d275910 实现内部混音总线基础架构（当前基线）
    a2cf0ce 修复音频来源列表刷新
    bf00ffa 支持常见设备音频格式转换
    7bd1076 修复音频引擎核心验收与生命周期问题

工作区目前只允许存在用户自己的未跟踪目录 .workbuddy/。不要修改、删除、暂存或提交它。Library/ 是本地参考资料目录，不属于 Git，也不允许读取后直接复制第三方代码进正式工程。旧 Slint 前端完整归档在 Library/LoopMaster-Slint-archive-2026-08-25/app/。

项目是 Windows 用户态音频路由器，不创建虚拟音频驱动。输出目标是系统中已经存在的物理设备或 VB-CABLE 等虚拟 endpoint。内部音频格式是 48 kHz / 32-bit IEEE float / stereo。

## 2. 已完成能力

### 音频内核和 WASAPI

- audio-core：固定容量 SPSC FIFO、固定 block 混音、增益、静音、channel map、路由图校验、不可变快照、平台无关重采样器、测试音。
- audio-windows：普通 Capture、Device Loopback、Process Loopback、Render sink、设备枚举、HRESULT/endpoint 错误、设备失效状态、自动重连状态机。
- 支持 16-bit PCM 和 32-bit IEEE float。
- 支持 44.1 kHz、48 kHz 及其他有效采样率的边界重采样。
- 支持单声道、双声道和常见多声道 endpoint 的边界转换：Capture 下混到内部 stereo；Render 默认只写 FL/FR，其余物理声道静音。
- WAVEFORMATEX / WAVEFORMATEXTENSIBLE 已校验 block alignment、平均字节率、subformat 和 valid bits。
- Capture 转换缓冲和重采样输入缓冲已预分配，容量不足会显式失败，不允许热路径无界扩容。
- 运行时支持多 source、多 sink 和多 send；拓扑变化运行中仍要求停止后重建，send 级增益、静音、channel map 可在 block 边界更新。

### 应用服务和前端

- app-service 已提供设备模型、进程模型、路由编辑器、引擎创建/启动/停止/更新图的 M1 API。
- 旧 Slint UI 曾可枚举 Process Loopback 来源和 Render 输出设备，现已归档；当前没有正式可运行的前端。
- 来源列表是当前存在 WASAPI 音频会话的进程，不是所有 Windows 进程。
- 已修复来源列表只在启动时枚举一次的问题：顶部有“刷新音源”按钮，枚举在后台线程执行，UI 定时器只接收结果，不执行 WASAPI 枚举。
- 刷新按 PID 和 endpoint ID 保留选择；进程退出或设备消失时清除选择；刷新失败保留旧列表。
- 旧 UI 仍是单 source、单 sink 的 MVP 交互；虽然底层模型支持多路由，当前 React 前端尚未初始化。

### 已验证结果

迁移分支当前门禁状态：

    cargo metadata --no-deps     通过，workspace 不再包含 app/ Slint crate
    cargo check --workspace      未通过，diagnostics 仍按旧 SendSpec/RouteGraph 初始化
    cargo test --workspace       尚未执行通过，必须先修复上述编译错误

历史真实设备短测已覆盖：44.1 kHz capture 到 48 kHz render、48 kHz capture 到 44.1 kHz render、单声道 capture 到双声道 render；这些结果来自前置分支，不等于当前总线图分支已通过全部门禁。

真实 16-bit PCM、真实 5.1/7.1 impulse/channel-id、Device Loopback/Process Loopback 长时间矩阵测试仍未完成，不能宣称已经通过。

## 3. 当前已知限制和风险

1. 进程来源只能是有 WASAPI 音频会话的程序。没有播放音频的程序不会出现在列表，这是设计边界，不要改成枚举全部进程后假装都可捕获。
2. 当前刷新是手动触发。后续可以接入 WASAPI 音频会话通知或低频自动刷新，但必须保持后台执行，不能在 React/Tauri UI 线程调用设备枚举。
3. SendSpec 目前还没有独立的 enabled 字段；muted 只能表示静音。Doc/Main/8.应用服务接口设计.md 中关于 enabled、事件订阅、配置 schema 的内容是目标契约，不代表代码已完成。
4. app-service::EngineService 当前是简单的同步包装，尚未实现完整的 EngineCommand、ServiceEvent、订阅机制和手动重连 API。
5. 配置/预设保存、加载、schema version、原子写入、缺失设备标记尚未实现。
6. 旧 Slint UI 已归档；Tauri 2 + React 前端尚未初始化。新前端必须通过 Tauri command/event 进入 Rust 应用服务，不能把旧 UI 回调逻辑直接搬过去。
7. 运行中 source/sink 拓扑变化会返回“需要重启”；UI 当前通过停机并重建服务完成切换。完整服务层应把这个行为做成明确的用户状态和命令。
8. 多声道转换不是动态多声道路由。若未来要让用户把输入的任意物理声道独立发送到输出的任意物理声道，必须重新设计动态声道数、channel map、FIFO、Mixer 和 UI，不能只放宽兼容判断。
9. Process Loopback 的显式格式请求依赖 Windows 系统/驱动接受该格式，Initialize 失败必须报告，不得假设所有系统必然支持。
10. 旧 Slint 组件已归档，不再参与构建；不要为了迁移方便重新把旧组件加入 workspace。新前端的 React/Tauri 警告应单独处理。

## 4. 下一阶段开发路线

按照原开发路线，音频内核 Phase 2 和设备恢复/路由图的主要骨架已经完成，旧 Slint 前端已归档，当前应进入“前端迁移准备和 Tauri 2 + React MVP”，同时补齐应用服务契约和配置基础设施，而不是立即扩展 ASIO、VST 或自有虚拟驱动。

### 第一步：冻结并实现应用服务契约

目标文件主要是 audio-core/src/lib.rs、app-service/src/lib.rs、必要时 audio-windows/src/runtime.rs。

任务：

- 给 SendSpec 增加 enabled: bool，明确 enabled=false 与 muted=true 的不同语义；
- 扩展 RouteEdit：创建/删除/启用/禁用 send，修改增益、静音、channel map；
- 实现 EngineCommand，至少覆盖 Start、Stop、ApplyRoute、SetGain、SetMuted、SetSendEnabled；
- 实现结构化 ServiceEvent：状态变化、统计变化、设备丢失、设备恢复；
- 实现服务层统一错误分类，保留 HRESULT、endpoint ID 和中文恢复建议；
- 实现 request_reconnect() 或等价的明确重试命令；
- 保持 UI 不接触 WASAPI 类型和 AudioEngine worker。

验收：服务层单元测试覆盖非法命令不改变状态、send 启停与 muted 区分、路由快照在 block 边界生效、设备错误字段完整。

### 第二步：实现配置和预设

目标主要是 app-service 新增配置模块和测试。

任务：

- 定义 AppConfig { schema_version, graph, ui_state }；
- JSON 序列化/反序列化，显式版本校验和未来迁移入口；
- 只持久化稳定 endpoint ID，不持久化设备列表索引；
- 设备缺失时保留路由并标记 unavailable，不按名称自动替换；
- 使用临时文件写入后原子替换；
- 对畸形 JSON、未知版本、重复 ID、缺失 endpoint、非法增益编写测试；
- 设计“加载后待用户确认”策略，禁止配置加载直接悄悄启动音频。

验收：保存/加载 round-trip、旧 schema 拒绝或迁移、设备消失后配置仍可读、写入中断不会破坏旧文件。

### 第三步：实现 Tauri command/event 与服务的命令/事件闭环

目标主要是 frontend/、新生成的 frontend/src-tauri/ 和 app-service。旧 app/ 不应重新恢复。

任务：

- UI 只维护展示模型和用户意图；
- 后台服务线程执行引擎启动、停止、路由提交、重连和设备刷新；
- UI 使用结构化事件更新 Running、Degraded、Reconnecting、Failed、Stopped；
- 统计刷新使用有界消息/快照，不让 UI 轮询直接读取实时内部结构；
- 设备/进程列表有 loading、empty、unavailable、error 状态；
- 保留当前“刷新音源”按钮，后续可增加低频自动刷新或 WASAPI 会话通知；
- 路由拓扑变化必须向用户显示“需要重启”，不能悄悄丢弃修改。

验收：UI 操作不阻塞音频线程；设备拔出后 UI 显示 degraded/reconnecting；恢复后显示 restored；错误包含可执行建议；切换列表顺序不会改变已选 endpoint。

### 第四步：完成真正的 Tauri 2 + React MVP

任务：

- 在一个主路由工作区实现多 source、多 sink、多 send 的可视化路由表；
- 每条 send 的启用、静音、增益和 channel map；
- 在主路由工作区展示设备流向、格式、声道、兼容性和缺失状态；
- 展示实时状态、峰值、packet/frame、FIFO、discontinuity、重连次数；
- 所有界面文本使用中文；
- 旧 Slint 组件只保留在 Library，不再作为新前端实现依据。

预设页、独立诊断页和其他工作区属于 MVP 之后的迭代，不得在前端初始化阶段擅自扩大范围。

验收流程必须能在不查看命令行日志的情况下完成“音频应用 → VB-CABLE CABLE Input → 录音/会议应用选择 CABLE Output”的完整流程。

### 第五步：真实硬件门禁

在功能和服务契约稳定后执行，不要用 UI 开发掩盖未验证的音频问题：

- 真实 16-bit PCM capture/render；
- 44.1/48 kHz 虚拟设备和物理 USB 声卡；
- 单声道、双声道、5.1/7.1 endpoint；
- Device Loopback、Process Loopback；
- VB-CABLE 闭环到录音/会议应用；
- 30 分钟、2 小时连续运行；
- 延迟、CPU、FIFO 深度和错误计数；
- 拔插 VB-CABLE/物理声卡并记录恢复时间和状态转移。

每次实机测试必须在 Doc/Log/YYYY-MM-DD-主题.md 记录 Windows 版本、设备、驱动、endpoint ID、采样率、声道、block/FIFO 配置、时长、原始输出和结论。

## 5. 下一位 agent 的第一轮执行要求

不要直接开始写 UI。第一轮应：

1. 阅读本文、Doc/Main/1-8、最近三份 Doc/Log；
2. 检查 `git status --short --branch` 和 `git log`，确认当前不在 `main` 上直接开发，并确认目标基线和未提交修改；
3. 创建专用分支，例如 codex/feature-app-service-contract；
4. 先审查 SendSpec、RouteEditor、EngineService、AudioEngine::update_graph 的真实接口与文档差异；
5. 将第一阶段范围限定为服务契约，不夹带预设、UI 大改或格式引擎重构；
6. 让子 agent 分别承担实现、测试/审查、文档记录，主 agent 负责拆分、验收和合并；
7. 每个逻辑问题使用独立中文提交，完成后先跑自动检查，再合并到 main；
8. 完成后更新 Doc/Log，说明已完成、未完成、验证结果和下一步。

## 6. Git 和协作硬规则

- 禁止直接在 main 开发、提交或修改后提交；
- 分支必须使用 codex/ 前缀和有意义的英文主题；
- 提交信息的 summary 必须是中文；
- 合并前必须运行适用的 cargo fmt、cargo clippy、cargo test、Windows target 检查和 git diff --check；
- 不得使用 git reset --hard、git checkout -- 覆盖用户修改；
- 不得处理、删除或提交 .workbuddy/；
- 不得把 Library/ 加入 Git 或作为正式源码依赖；
- 不得把未做实机验证的能力写成“已支持”；
- 不得因为用户想要“识别所有程序”就枚举全部进程并虚构音频来源；必须保持“有 WASAPI 音频会话才可 Process Loopback”的事实边界；
- 不得为了方便把 WASAPI 指针、COM 对象或 UI 半成品状态泄漏到实时混音层。

## 7. 常用命令

    # 查看分支和工作区
    git status --short --branch
    git log --oneline --decorate -10

    # 常规验证
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace
    cargo check --workspace --target x86_64-pc-windows-msvc
    git diff --check

    # 枚举当前 Windows endpoint 和音频会话
    cargo run -p loopmaster-diagnostics -- --list-endpoints
    cargo run -p loopmaster-diagnostics -- --processes

    # 严格稳定性测试示例，ID 必须替换为当前机器真实 ID
    cargo run -p loopmaster-diagnostics -- --engine "<capture-id>" "<render-id>" 300

    # 设备拔插恢复测试
    cargo run -p loopmaster-diagnostics -- --recovery-engine "<capture-id>" "<render-id>" 300

    # 初始化完成后启动 Tauri + React 前端
    cd frontend
    npm run tauri dev

## 8. 结束条件

下一轮不能以“代码能编译”作为完成标准。至少应同时满足：

- 服务契约和代码接口一致；
- 自动测试覆盖正常、错误、恢复和不变性；
- UI 不阻塞实时线程；
- 文档和日志与实际实现一致；
- Git 分支、中文提交和合并流程符合规范；
- 未验证能力明确写入剩余风险，而不是隐藏或夸大。
