//! Windows 音频平台适配层的骨架。
//!
//! 当前只定义能力和生命周期边界；设备打开、捕获、混音和渲染尚未实现。

use loopmaster_audio_core::{AudioFormat, EndpointId};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointFlow {
    Capture,
    Render,
}

#[derive(Clone, Debug)]
pub struct EndpointInfo {
    pub id: EndpointId,
    pub name: String,
    pub flow: EndpointFlow,
    pub format: Option<AudioFormat>,
}

#[derive(Debug, Error)]
pub enum WindowsAudioError {
    #[error("Windows 音频后端尚未实现")]
    NotImplemented,
}

pub struct WindowsAudioBackend;

impl WindowsAudioBackend {
    pub fn new() -> Result<Self, WindowsAudioError> {
        Ok(Self)
    }

    pub fn enumerate_endpoints(&self) -> Result<Vec<EndpointInfo>, WindowsAudioError> {
        Err(WindowsAudioError::NotImplemented)
    }
}

