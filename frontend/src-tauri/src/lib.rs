//! LoopMaster 前端 Tauri 壳层。
//!
//! 这是 Tauri 自带的 Rust 壳层，是前端调用 Rust 应用服务（app-service）的
//! 唯一命令边界。React 只维护展示模型和用户意图；WASAPI 枚举、引擎控制都在
//! Tauri command 执行的后台线程完成，不阻塞 UI 主线程，也不把实时音频结构
//! 暴露给前端。
//!
//! 本阶段（feature-tauri-init）只建立最小启动闭环与路径依赖验证，不实现完整
//! 路由页面。`list_devices` 用于验证 src-tauri 能通过路径依赖访问 app-service
//! 的最小测试接口；后续阶段再扩展命令与事件闭环。

use serde::Serialize;

use loopmaster_app_service::{DeviceFlow, DeviceRepository};

// ---------------------------------------------------------------------------
// 前端 DTO（稳定、可审查，不直接暴露 Windows 类型）
// ---------------------------------------------------------------------------

/// 设备概要：仅投影稳定 ID、名称与流向，供壳层验证与后续列表渲染。
#[derive(Serialize)]
struct DeviceBrief {
    id: String,
    name: String,
    flow: &'static str,
}

impl DeviceBrief {
    fn from_model(model: &loopmaster_app_service::DeviceModel) -> Self {
        Self {
            id: model.id.0.clone(),
            name: model.name.clone(),
            flow: match model.flow {
                DeviceFlow::Capture => "capture",
                DeviceFlow::Render => "render",
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// 连通性测试：验证 Tauri IPC 与 React 之间的调用链路。
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// 枚举设备（只读、后台执行）。通过 app-service 的 `DeviceRepository` 访问
/// Windows 音频后端，是 src-tauri 对 app-service 路径依赖的最小验证。
#[tauri::command]
fn list_devices() -> Result<Vec<DeviceBrief>, String> {
    let repository = DeviceRepository::new().map_err(|e| e.to_string())?;
    let devices = repository.list_devices().map_err(|e| e.to_string())?;
    Ok(devices.iter().map(DeviceBrief::from_model).collect())
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet, list_devices])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
