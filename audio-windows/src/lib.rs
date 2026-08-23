//! Windows 音频平台适配层。
//!
//! Windows 音频平台适配层和正式音频引擎运行时。

use loopmaster_audio_core::{
    AudioFormat, EndpointId, FixedInputResampler, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE,
};
use thiserror::Error;

mod process_loopback;
mod runtime;

pub use process_loopback::ProcessLoopbackSource;
pub use runtime::{
    AudioEngine, AudioEngineConfig, AudioEngineError, AudioEngineState, AudioEngineStats,
    AudioEngineStatus,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Windows 音频 endpoint 的数据流方向。
pub enum EndpointFlow {
    Capture,
    Render,
}

impl EndpointFlow {
    /// 返回稳定的机器可读方向名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Render => "render",
        }
    }
    /// 返回面向诊断输出的方向名称。
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Capture => "Capture",
            Self::Render => "Render",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// `IAudioClient::GetMixFormat` 返回的音频格式摘要。
pub struct EndpointFormat {
    /// 每秒采样帧数。
    pub sample_rate: u32,
    /// 每个采样的有效位深。
    pub bits_per_sample: u16,
    /// 交错音频中的声道数。
    pub channels: u16,
    /// `WAVEFORMATEXTENSIBLE.dwChannelMask`；普通 `WAVEFORMATEX` 为 0。
    pub channel_mask: u32,
    /// 样本是否为 IEEE 32-bit float（`WAVE_FORMAT_IEEE_FLOAT` 或其
    /// extensible subformat）。这是 LoopMaster 内部格式契约的一部分，
    /// 用于在打开设备前判断该 endpoint 能否作为 source 或 sink。
    pub is_float: bool,
}

impl EndpointFormat {
    /// 将 Windows 格式摘要转换为平台无关的格式模型。
    pub const fn audio_format(self) -> AudioFormat {
        AudioFormat {
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
    }

    /// 该格式能否作为 LoopMaster 的普通 capture source。
    ///
    /// 普通 WASAPI capture 严格要求 48 kHz、32-bit IEEE float、双声道，
    /// 与 [`super::open_capture_source`] 的格式校验保持一致。不满足的
    /// endpoint 会在打开时返回 `CaptureFormatUnsupported`，这里提前暴露
    /// 相同的判断，供诊断和 UI 标注可用性。
    pub const fn capture_compatible(self) -> bool {
        self.is_float
            && self.bits_per_sample == 32
            && self.channels == INTERNAL_CHANNELS as u16
            && self.sample_rate == INTERNAL_SAMPLE_RATE
    }

    /// 该格式能否作为 LoopMaster 的 render sink。
    ///
    /// render sink 要求 32-bit IEEE float、双声道；采样率与内部 48 kHz 不一致时
    /// 由 [`FixedInputResampler`] 在写入边界转换，因此这里不要求采样率。
    pub const fn render_compatible(self) -> bool {
        self.is_float && self.bits_per_sample == 32 && self.channels == INTERNAL_CHANNELS as u16
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// 一个处于 active 状态的 Windows 音频 endpoint。
pub struct EndpointInfo {
    /// WASAPI endpoint 的稳定设备 ID。
    pub id: EndpointId,
    /// `PKEY_Device_FriendlyName` 返回的友好名称。
    pub name: String,
    /// endpoint 的捕获或渲染方向。
    pub flow: EndpointFlow,
    /// 采样率和声道数；读取格式失败时为 `None`。
    pub format: Option<AudioFormat>,
    /// 位深；读取格式失败时为 `None`。
    pub bits_per_sample: Option<u16>,
    /// channel mask；读取格式失败时为 `None`。
    pub channel_mask: Option<u32>,
    /// 样本是否为 IEEE float；读取格式失败时为 `None`。
    pub is_float: Option<bool>,
}

/// 当前存在播放音频会话的进程，可直接作为 Process Loopback 的目标。
///
/// 该列表来自 WASAPI `IAudioSessionManager2`，不是所有进程的快照；因此没有
/// 音频会话的进程不会出现在列表中。进程可能在枚举和读取名称之间退出，调用方
/// 必须在创建 source 时再次处理 PID 无效错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Windows 进程 ID。
    pub pid: u32,
    /// 进程可读名称。无法读取名称时包含 PID 和原因，但 PID 仍可用于重试。
    pub name: String,
    /// 进程可执行文件的完整路径；权限不足时为 `None`。
    pub executable_path: Option<String>,
}

impl EndpointInfo {
    /// 在格式字段完整时返回一个便于输出的格式摘要。
    pub fn endpoint_format(&self) -> Option<EndpointFormat> {
        match (
            self.format,
            self.bits_per_sample,
            self.channel_mask,
            self.is_float,
        ) {
            (Some(format), Some(bits_per_sample), Some(channel_mask), Some(is_float)) => {
                Some(EndpointFormat {
                    sample_rate: format.sample_rate,
                    bits_per_sample,
                    channels: format.channels,
                    channel_mask,
                    is_float,
                })
            }
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
/// Windows/WASAPI 操作错误，保留 HRESULT 和相关 endpoint ID。
pub enum WindowsAudioError {
    #[error("Windows 音频 endpoint 仅支持 Windows 平台")]
    UnsupportedPlatform,
    #[error("Process Loopback 进程 ID 无效: {pid}")]
    ProcessLoopbackInvalidPid { pid: u32 },
    #[error("COM 初始化失败: HRESULT=0x{hresult:08X}")]
    ComInitialization { hresult: i32 },
    #[error("WASAPI 操作 {operation} 失败: HRESULT=0x{hresult:08X}, endpoint={endpoint_id:?}")]
    HResult {
        operation: &'static str,
        hresult: i32,
        endpoint_id: Option<String>,
    },
    #[error("endpoint 音频格式无效: {reason}, endpoint={endpoint_id:?}")]
    InvalidFormat {
        reason: String,
        endpoint_id: Option<String>,
    },
    #[error("render endpoint 格式不满足 MVP 要求（32-bit IEEE float、2 声道）: endpoint={endpoint_id}, {sample_rate} Hz, {bits_per_sample} bit, {channels} channels")]
    RenderFormatUnsupported {
        endpoint_id: String,
        sample_rate: u32,
        bits_per_sample: u16,
        channels: u16,
    },
    #[error("render block frame 数无效: {frames}")]
    RenderBlockFramesInvalid { frames: usize },
    #[error("render 输入样本数 {samples} 不能按 {channels} 声道对齐")]
    RenderInputUnaligned { samples: usize, channels: usize },
    #[error("render 输入 block 为 {actual} frame，超过计划的 {expected} frame")]
    RenderBlockTooLarge { expected: usize, actual: usize },
    #[error("render endpoint 缓冲区状态无效: {reason}, endpoint={endpoint_id}")]
    RenderState {
        reason: &'static str,
        endpoint_id: String,
    },
    #[error("capture endpoint 格式不满足 MVP 要求（48 kHz、32-bit IEEE float、2 声道）: endpoint={endpoint_id}, {sample_rate} Hz, {bits_per_sample} bit, {channels} channels")]
    CaptureFormatUnsupported {
        endpoint_id: String,
        sample_rate: u32,
        bits_per_sample: u16,
        channels: u16,
    },
    #[error("capture endpoint 缓冲区状态无效: {reason}, endpoint={endpoint_id}")]
    CaptureState {
        reason: &'static str,
        endpoint_id: String,
    },
    #[error("render 重采样失败: {reason}, endpoint={endpoint_id}")]
    RenderResample { reason: String, endpoint_id: String },
}

/// WASAPI 错误的恢复分类。
///
/// 设备失效通常可以通过重新枚举并重建 stream 恢复；其他错误必须先由
/// 上层修正配置或实现对应的功能，不能未经判断自动重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsAudioFailureKind {
    DeviceUnavailable,
    Other,
}

/// `AUDCLNT_E_DEVICE_INVALIDATED`。
pub const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;
/// `AUDCLNT_E_SERVICE_NOT_RUNNING`（Windows SDK 定义值）。
pub const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010u32 as i32;
/// 早期资料中曾将 `0x88890005` 标为服务不可用；为兼容外部诊断结果，仍将
/// 该值视为设备不可用，但不把它冒充为当前 SDK 的正式常量。
pub const AUDCLNT_E_SERVICE_NOT_RUNNING_LEGACY: i32 = 0x8889_0005u32 as i32;
/// `AUDCLNT_E_ENDPOINT_CREATE_FAILED`。
pub const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000Fu32 as i32;
/// Win32 `ERROR_DEVICE_NOT_CONNECTED` 的 HRESULT 形式。
pub const HRESULT_ERROR_DEVICE_NOT_CONNECTED: i32 = 0x8007_048Fu32 as i32;
/// Win32 `ERROR_NOT_FOUND` 的 HRESULT 形式，常见于设备已被拔出后按旧 ID 获取。
pub const HRESULT_ERROR_NOT_FOUND: i32 = 0x8007_0490u32 as i32;

/// WASAPI shared-mode render 写入结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderWriteResult {
    /// 已写入指定 frame；当设备可用空间少于 block 时可能小于计划 block。
    Written { frames: u32 },
    /// 当前 padding 已占满设备缓冲区，本次没有阻塞等待或写入。
    NoSpace,
}

/// WASAPI shared-mode render sink。
///
/// 该类型只在 Windows 上持有 COM 接口；非 Windows 平台仍可引用公开 API，
/// 但构造和写入会返回 [`WindowsAudioError::UnsupportedPlatform`]。
pub struct WasapiRenderSink {
    #[cfg(windows)]
    _com: ComGuard,
    #[cfg(windows)]
    client: windows::Win32::Media::Audio::IAudioClient,
    #[cfg(windows)]
    render_client: windows::Win32::Media::Audio::IAudioRenderClient,
    endpoint_id: EndpointId,
    format: EndpointFormat,
    buffer_frames: u32,
    block_frames: usize,
    resampler: Option<FixedInputResampler>,
    resampled_output: Vec<f32>,
}

/// 一次 WASAPI capture packet 的元数据。
///
/// `data` 仅在 `WasapiCaptureSource::drain_packets` 的回调期间有效；`silent`
/// 为真时没有有效样本数据，调用方必须自行写入对应长度的静音帧。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturePacket {
    pub frames: u32,
    pub silent: bool,
    pub discontinuity: bool,
    pub timestamp_error: bool,
}

/// 一次非阻塞 capture 排空的统计结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureDrainResult {
    pub packets: u64,
    pub frames: u64,
    pub silent_packets: u64,
    pub discontinuities: u64,
    pub timestamp_errors: u64,
}

/// WASAPI shared-mode 普通 capture source。
pub struct WasapiCaptureSource {
    #[cfg(windows)]
    _com: ComGuard,
    #[cfg(windows)]
    client: windows::Win32::Media::Audio::IAudioClient,
    #[cfg(windows)]
    capture_client: windows::Win32::Media::Audio::IAudioCaptureClient,
    endpoint_id: EndpointId,
    format: EndpointFormat,
}

#[cfg(windows)]
impl Drop for WasapiRenderSink {
    fn drop(&mut self) {
        // Drop 不能向调用方返回 HRESULT；尽力停止流后由 COM 释放接口。
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(windows)]
impl Drop for WasapiCaptureSource {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

impl WindowsAudioError {
    /// 返回错误携带的原始 HRESULT。
    pub const fn hresult(&self) -> Option<i32> {
        match self {
            Self::ComInitialization { hresult } | Self::HResult { hresult, .. } => Some(*hresult),
            _ => None,
        }
    }
    /// 返回错误关联的 endpoint ID（如果已经取得）。
    pub fn endpoint_id(&self) -> Option<&str> {
        match self {
            Self::HResult { endpoint_id, .. } | Self::InvalidFormat { endpoint_id, .. } => {
                endpoint_id.as_deref()
            }
            Self::RenderFormatUnsupported { endpoint_id, .. }
            | Self::RenderState { endpoint_id, .. } => Some(endpoint_id),
            Self::CaptureFormatUnsupported { endpoint_id, .. }
            | Self::CaptureState { endpoint_id, .. } => Some(endpoint_id),
            Self::RenderResample { endpoint_id, .. } => Some(endpoint_id),
            _ => None,
        }
    }

    /// 判断错误是否属于设备暂时不可用，可进入自动恢复流程。
    pub fn failure_kind(&self) -> WindowsAudioFailureKind {
        if matches!(
            self,
            Self::CaptureState {
                reason: "endpoint 不是 active 状态",
                ..
            } | Self::RenderState {
                reason: "endpoint 不是 active 状态",
                ..
            }
        ) {
            return WindowsAudioFailureKind::DeviceUnavailable;
        }
        match self.hresult() {
            Some(
                AUDCLNT_E_DEVICE_INVALIDATED
                | AUDCLNT_E_SERVICE_NOT_RUNNING
                | AUDCLNT_E_SERVICE_NOT_RUNNING_LEGACY
                | AUDCLNT_E_ENDPOINT_CREATE_FAILED
                | HRESULT_ERROR_DEVICE_NOT_CONNECTED
                | HRESULT_ERROR_NOT_FOUND,
            ) => WindowsAudioFailureKind::DeviceUnavailable,
            _ => WindowsAudioFailureKind::Other,
        }
    }

    /// 判断错误是否由设备失效导致。
    pub fn is_device_failure(&self) -> bool {
        self.failure_kind() == WindowsAudioFailureKind::DeviceUnavailable
    }
}

/// 解码 `WAVEFORMATEX/WAVEFORMATEXTENSIBLE` 的原始小端字节。
///
/// 输入由 `18 + cbSize` 个字节组成，适用于 `GetMixFormat` 返回的结构副本。
pub fn decode_mix_format(raw: &[u8]) -> Result<EndpointFormat, WindowsAudioError> {
    if raw.len() < 18 {
        return Err(invalid_format("WAVEFORMATEX 长度不足", None));
    }
    let format_tag = u16::from_le_bytes([raw[0], raw[1]]);
    let channels = u16::from_le_bytes([raw[2], raw[3]]);
    let sample_rate = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let bits_per_sample = u16::from_le_bytes([raw[14], raw[15]]);
    let cb_size = u16::from_le_bytes([raw[16], raw[17]]) as usize;
    if channels == 0 {
        return Err(invalid_format("声道数为 0", None));
    }
    if sample_rate == 0 {
        return Err(invalid_format("采样率为 0", None));
    }
    if raw.len() < 18 + cb_size {
        return Err(invalid_format("cbSize 超出返回缓冲区", None));
    }
    let channel_mask = if format_tag == 0xFFFE && cb_size >= 22 {
        if raw.len() < 24 {
            return Err(invalid_format("extensible 格式缺少 channel mask", None));
        }
        u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]])
    } else {
        0
    };
    // `WAVE_FORMAT_IEEE_FLOAT` (0x0003) 直接表示 32-bit float；extensible 格式
    // 需要读取 subformat GUID 与 `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` 比较。
    // 该结果供调用方在打开设备前判断格式契约，不参与原始字段解码之外的任何操作。
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT 的 GUID 为
    // 00000003-0000-0010-8000-00AA00389B71，按其 Windows GUID 内存布局展开。
    const IEEE_FLOAT_SUBFORMAT: [u8; 16] = [
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ];
    let is_float = if format_tag == 3 {
        true
    } else if format_tag == 0xFFFE && cb_size >= 22 && raw.len() >= 40 {
        raw[24..40] == IEEE_FLOAT_SUBFORMAT
    } else {
        false
    };
    Ok(EndpointFormat {
        sample_rate,
        bits_per_sample,
        channels,
        channel_mask,
        is_float,
    })
}

fn invalid_format(reason: &str, endpoint_id: Option<String>) -> WindowsAudioError {
    WindowsAudioError::InvalidFormat {
        reason: reason.to_owned(),
        endpoint_id,
    }
}

#[cfg(windows)]
fn enumerate_processes_windows() -> Result<Vec<ProcessInfo>, WindowsAudioError> {
    use std::collections::BTreeMap;
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| hresult_error("CoCreateInstance(MMDeviceEnumerator)", None, error))?;
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|error| hresult_error("IMMDeviceEnumerator::EnumAudioEndpoints", None, error))?;
    let count = unsafe { collection.GetCount() }
        .map_err(|error| hresult_error("IMMDeviceCollection::GetCount", None, error))?;
    let mut processes = BTreeMap::new();

    for index in 0..count {
        let device = unsafe { collection.Item(index) }
            .map_err(|error| hresult_error("IMMDeviceCollection::Item", None, error))?;
        let endpoint_id = unsafe { get_endpoint_id(&device) }?;
        let manager: IAudioSessionManager2 =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|error| {
                hresult_error(
                    "IMMDevice::Activate(IAudioSessionManager2)",
                    Some(endpoint_id.clone()),
                    error,
                )
            })?;
        let sessions = unsafe { manager.GetSessionEnumerator() }.map_err(|error| {
            hresult_error(
                "IAudioSessionManager2::GetSessionEnumerator",
                Some(endpoint_id.clone()),
                error,
            )
        })?;
        let session_count = unsafe { sessions.GetCount() }.map_err(|error| {
            hresult_error(
                "IAudioSessionEnumerator::GetCount",
                Some(endpoint_id.clone()),
                error,
            )
        })?;
        for session_index in 0..session_count {
            let control = match unsafe { sessions.GetSession(session_index) } {
                Ok(control) => control,
                // Session lists are mutable. A session can disappear between GetCount
                // and GetSession; ignore that transient race and continue enumeration.
                Err(_) => continue,
            };
            let control = match control.cast::<IAudioSessionControl2>() {
                Ok(control) => control,
                Err(_) => continue,
            };
            let pid = match unsafe { control.GetProcessId() } {
                Ok(pid) if pid != 0 => pid,
                // PID 0 is the system-sounds session and cannot be a Process Loopback
                // target. Other failures commonly mean that the session just exited.
                _ => continue,
            };
            if processes.contains_key(&pid) {
                continue;
            }
            if let Some(info) = process_info(pid) {
                processes.insert(pid, info);
            }
        }
    }
    Ok(processes.into_values().collect())
}

#[cfg(windows)]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        Err(error) => {
            let code = error.code().0 as u32;
            // ERROR_INVALID_PARAMETER / ERROR_NOT_FOUND indicates that the process
            // exited during enumeration. Access denied is retained with a fallback name.
            if matches!(code, 0x8007_0057 | 0x8007_0490 | 0x8007_0002) {
                return None;
            }
            return Some(ProcessInfo {
                pid,
                name: format!("PID {pid}（无法读取名称：权限不足）"),
                executable_path: None,
            });
        }
    };
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let path = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    let path = match path {
        Ok(()) if length > 0 => String::from_utf16_lossy(&buffer[..length as usize]),
        Err(error) => {
            let code = error.code().0 as u32;
            if matches!(code, 0x8007_0057 | 0x8007_0490 | 0x8007_0002) {
                return None;
            }
            return Some(ProcessInfo {
                pid,
                name: format!("PID {pid}（无法读取名称：权限不足）"),
                executable_path: None,
            });
        }
        _ => {
            return Some(ProcessInfo {
                pid,
                name: format!("PID {pid}（无法读取名称）"),
                executable_path: None,
            });
        }
    };
    let name = path
        .rsplit(['\\', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(&path)
        .to_owned();
    Some(ProcessInfo {
        pid,
        name,
        executable_path: Some(path),
    })
}

/// Windows 音频后端；当前仅提供 endpoint 静态诊断。
pub struct WindowsAudioBackend {
    #[cfg(windows)]
    _com: ComGuard,
}

impl WindowsAudioBackend {
    /// 初始化当前线程的 COM apartment。
    pub fn new() -> Result<Self, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            let result = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                )
            };
            let hresult = result.0;
            if hresult < 0 && hresult as u32 != 0x80010106 {
                return Err(WindowsAudioError::ComInitialization { hresult });
            }
            Ok(Self {
                _com: ComGuard {
                    should_uninitialize: hresult == 0 || hresult == 1,
                },
            })
        }
    }

    /// 枚举 active capture/render endpoint，并读取友好名称及默认混合格式。
    pub fn enumerate_endpoints(&self) -> Result<Vec<EndpointInfo>, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = self;
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            use windows::Win32::Media::Audio::{
                eCapture, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
            };
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                    .map_err(|e| hresult_error("CoCreateInstance(MMDeviceEnumerator)", None, e))?;
            let mut endpoints = Vec::new();
            for (dataflow, flow) in [
                (eRender, EndpointFlow::Render),
                (eCapture, EndpointFlow::Capture),
            ] {
                let collection =
                    unsafe { enumerator.EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE) }
                        .map_err(|e| {
                            hresult_error("IMMDeviceEnumerator::EnumAudioEndpoints", None, e)
                        })?;
                let count = unsafe { collection.GetCount() }
                    .map_err(|e| hresult_error("IMMDeviceCollection::GetCount", None, e))?;
                for index in 0..count {
                    let device = unsafe { collection.Item(index) }
                        .map_err(|e| hresult_error("IMMDeviceCollection::Item", None, e))?;
                    let endpoint_id = unsafe { get_endpoint_id(&device) }?;
                    let name = unsafe { get_endpoint_name(&device, &endpoint_id) }?;
                    let format = unsafe { get_endpoint_format(&device, &endpoint_id) }?;
                    endpoints.push(EndpointInfo {
                        id: EndpointId(endpoint_id),
                        name,
                        flow,
                        format: Some(format.audio_format()),
                        bits_per_sample: Some(format.bits_per_sample),
                        channel_mask: Some(format.channel_mask),
                        is_float: Some(format.is_float),
                    });
                }
            }
            Ok(endpoints)
        }
    }

    /// 枚举当前具有活动播放会话的进程。
    ///
    /// 每个进程只返回一次，即使它同时向多个 render endpoint 播放。读取进程
    /// 路径需要 `PROCESS_QUERY_LIMITED_INFORMATION`：权限不足时保留该 PID，
    /// 并返回可识别的占位名称；进程在枚举期间退出则忽略该会话的瞬时竞态。
    pub fn enumerate_processes(&self) -> Result<Vec<ProcessInfo>, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = self;
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            enumerate_processes_windows()
        }
    }

    /// 重新枚举并确认一个 endpoint 处于 active 状态且流向匹配。
    ///
    /// 设备拔插期间，Windows 可能暂时保留旧 endpoint ID，但其状态仍为
    /// `DEVICE_STATE_UNPLUGGED`/`DISABLED`。重连 supervisor 应在创建 WASAPI
    /// stream 前轮询此结果，避免反复启动必然失败的 worker。
    pub fn is_endpoint_active(
        &self,
        endpoint_id: &EndpointId,
        expected_flow: EndpointFlow,
    ) -> Result<bool, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = (self, endpoint_id, expected_flow);
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            use windows::core::Interface;
            use windows::Win32::Media::Audio::{
                eCapture, eRender, IMMDeviceEnumerator, IMMEndpoint, MMDeviceEnumerator,
                DEVICE_STATE_ACTIVE,
            };
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(
                    |error| hresult_error("CoCreateInstance(MMDeviceEnumerator)", None, error),
                )?;
            let expected = match expected_flow {
                EndpointFlow::Capture => eCapture,
                EndpointFlow::Render => eRender,
            };
            let wide_id: Vec<u16> = endpoint_id
                .0
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let device =
                match unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide_id.as_ptr())) } {
                    Ok(device) => device,
                    Err(error) if is_device_not_ready_hresult(error.code().0) => return Ok(false),
                    Err(error) => {
                        return Err(hresult_error(
                            "IMMDeviceEnumerator::GetDevice",
                            Some(endpoint_id.0.clone()),
                            error,
                        ));
                    }
                };
            let state = unsafe { device.GetState() }.map_err(|error| {
                hresult_error("IMMDevice::GetState", Some(endpoint_id.0.clone()), error)
            })?;
            if state != DEVICE_STATE_ACTIVE {
                return Ok(false);
            }
            let endpoint = device.cast::<IMMEndpoint>().map_err(|error| {
                hresult_error(
                    "IMMDevice::QueryInterface(IMMEndpoint)",
                    Some(endpoint_id.0.clone()),
                    error,
                )
            })?;
            let flow = unsafe { endpoint.GetDataFlow() }.map_err(|error| {
                hresult_error(
                    "IMMEndpoint::GetDataFlow",
                    Some(endpoint_id.0.clone()),
                    error,
                )
            })?;
            Ok(flow == expected)
        }
    }

    /// 重新枚举并确认一对 endpoint 都处于 active 状态（兼容旧签名）。
    pub fn are_endpoints_active(
        &self,
        capture_id: &EndpointId,
        render_id: &EndpointId,
    ) -> Result<bool, WindowsAudioError> {
        Ok(self.is_endpoint_active(capture_id, EndpointFlow::Capture)?
            && self.is_endpoint_active(render_id, EndpointFlow::Render)?)
    }

    /// 打开指定 render endpoint，初始化 WASAPI shared-mode client，并启动流。
    ///
    /// 当前实现要求 endpoint 的 `GetMixFormat` 为 32-bit IEEE float，因而实时写入
    /// 可以直接复制 `f32`。`block_frames` 只用于限制单次写入；设备缓冲区实际可用
    /// frame 数仍由 `GetCurrentPadding` 决定。
    pub fn open_render_sink(
        &self,
        endpoint_id: &EndpointId,
        block_frames: usize,
    ) -> Result<WasapiRenderSink, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = (self, endpoint_id, block_frames);
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            open_render_sink(endpoint_id, block_frames)
        }
    }

    /// 打开指定普通 capture endpoint，并启动 shared-mode 流。
    pub fn open_capture_source(
        &self,
        endpoint_id: &EndpointId,
    ) -> Result<WasapiCaptureSource, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = (self, endpoint_id);
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            open_capture_source(endpoint_id)
        }
    }

    /// 打开指定 render endpoint 的 Device Loopback 捕获（该设备的播放总混音）。
    ///
    /// 当前与普通 capture 一样要求 48 kHz / 32-bit float / 2 声道的内部契约；
    /// 非 48 kHz 设备（如 44.1 kHz 虚拟声卡）的 loopback 重采样在阶段 B.5 补充。
    pub fn open_device_loopback_source(
        &self,
        endpoint_id: &EndpointId,
    ) -> Result<WasapiCaptureSource, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = (self, endpoint_id);
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            open_device_loopback_source(endpoint_id)
        }
    }
}

impl WasapiRenderSink {
    /// endpoint ID。
    pub fn endpoint_id(&self) -> &EndpointId {
        &self.endpoint_id
    }

    /// endpoint 的 f32 mix format。
    pub const fn format(&self) -> EndpointFormat {
        self.format
    }

    /// 设备实际 render buffer 的 frame 容量。
    pub const fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 本 sink 单次允许写入的最大 frame 数。
    pub const fn block_frames(&self) -> usize {
        self.block_frames
    }

    /// 将一个 interleaved f32 block 非阻塞写入设备。
    ///
    /// 输入不足一个计划 block 时，剩余部分写入静音；输入超过计划 block 或
    /// 不能按 endpoint 声道数对齐时返回错误。设备当前没有空间时返回 `NoSpace`。
    /// 该函数不分配、不加锁、不等待。
    pub fn write_f32_block(
        &mut self,
        samples: &[f32],
    ) -> Result<RenderWriteResult, WindowsAudioError> {
        #[cfg(not(windows))]
        {
            let _ = samples;
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            write_render_block(self, samples)
        }
    }
}

impl WasapiCaptureSource {
    pub fn endpoint_id(&self) -> &EndpointId {
        &self.endpoint_id
    }

    pub const fn format(&self) -> EndpointFormat {
        self.format
    }

    /// 读取当前全部可用 packet，并在 buffer 有效期内同步调用 `on_packet`。
    ///
    /// 此函数不等待、不分配；回调不可保留 `data`，应直接写入 source FIFO。
    /// `data` 在 silent packet 时为 `None`，否则是 interleaved `f32` 样本。
    pub fn drain_packets<F>(
        &mut self,
        mut on_packet: F,
    ) -> Result<CaptureDrainResult, WindowsAudioError>
    where
        F: FnMut(CapturePacket, Option<&[f32]>),
    {
        #[cfg(not(windows))]
        {
            let _ = &mut on_packet;
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            drain_capture_packets(self, &mut on_packet)
        }
    }
}

#[cfg(windows)]
fn open_render_sink(
    endpoint_id: &EndpointId,
    block_frames: usize,
) -> Result<WasapiRenderSink, WindowsAudioError> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, IMMEndpoint,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let com_result = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let com_hresult = com_result.0;
    if com_hresult < 0 && com_hresult as u32 != 0x80010106 {
        return Err(WindowsAudioError::ComInitialization {
            hresult: com_hresult,
        });
    }
    let com = ComGuard {
        should_uninitialize: com_hresult == 0 || com_hresult == 1,
    };

    if block_frames == 0 || block_frames > u32::MAX as usize {
        return Err(WindowsAudioError::RenderBlockFramesInvalid {
            frames: block_frames,
        });
    }

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|error| {
            hresult_error(
                "CoCreateInstance(MMDeviceEnumerator)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let wide_id: Vec<u16> = endpoint_id
        .0
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let device = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide_id.as_ptr())) }.map_err(
        |error| {
            hresult_error(
                "IMMDeviceEnumerator::GetDevice",
                Some(endpoint_id.0.clone()),
                error,
            )
        },
    )?;
    let state = unsafe { device.GetState() }.map_err(|error| {
        hresult_error("IMMDevice::GetState", Some(endpoint_id.0.clone()), error)
    })?;
    if state != DEVICE_STATE_ACTIVE {
        return Err(WindowsAudioError::RenderState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    let endpoint = device.cast::<IMMEndpoint>().map_err(|error| {
        hresult_error(
            "IMMDevice::QueryInterface(IMMEndpoint)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    let flow = unsafe { endpoint.GetDataFlow() }.map_err(|error| {
        hresult_error(
            "IMMEndpoint::GetDataFlow",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if flow != eRender {
        return Err(WindowsAudioError::RenderState {
            reason: "指定 endpoint 不是 render endpoint",
            endpoint_id: endpoint_id.0.clone(),
        });
    }

    let client: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|error| {
            hresult_error(
                "IMMDevice::Activate(IAudioClient)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let format_ptr = unsafe { client.GetMixFormat() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetMixFormat",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if format_ptr.is_null() {
        return Err(invalid_format(
            "IAudioClient::GetMixFormat 返回空指针",
            Some(endpoint_id.0.clone()),
        ));
    }

    let format_result = unsafe { inspect_render_mix_format(format_ptr, endpoint_id) };
    let format = match format_result {
        Ok(format) => format,
        Err(error) => {
            unsafe {
                windows::Win32::System::Com::CoTaskMemFree(Some(
                    format_ptr as *const core::ffi::c_void,
                ))
            };
            return Err(error);
        }
    };
    let initialize_result = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            format_ptr,
            None,
        )
    };
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void))
    };
    initialize_result.map_err(|error| {
        hresult_error(
            "IAudioClient::Initialize(shared)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;

    let buffer_frames = unsafe { client.GetBufferSize() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetBufferSize",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if buffer_frames == 0 {
        return Err(WindowsAudioError::RenderState {
            reason: "IAudioClient::GetBufferSize 返回 0",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    let (resampler, resampled_output) = if format.sample_rate == INTERNAL_SAMPLE_RATE {
        (None, Vec::new())
    } else {
        let resampler = FixedInputResampler::new(
            INTERNAL_SAMPLE_RATE,
            format.sample_rate,
            INTERNAL_CHANNELS,
            block_frames,
        )
        .map_err(|error| WindowsAudioError::RenderResample {
            reason: error.to_string(),
            endpoint_id: endpoint_id.0.clone(),
        })?;
        let output = vec![0.0; resampler.output_frames_max() * INTERNAL_CHANNELS];
        // Force allocation and construction before the realtime write path starts.
        let _ = resampler.output_frames_next();
        (Some(resampler), output)
    };
    let render_client: IAudioRenderClient = unsafe { client.GetService() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetService(IAudioRenderClient)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    unsafe { client.Start() }.map_err(|error| {
        hresult_error("IAudioClient::Start", Some(endpoint_id.0.clone()), error)
    })?;

    Ok(WasapiRenderSink {
        _com: com,
        client,
        render_client,
        endpoint_id: endpoint_id.clone(),
        format,
        buffer_frames,
        block_frames,
        resampler,
        resampled_output,
    })
}

#[cfg(windows)]
unsafe fn inspect_render_mix_format(
    format_ptr: *mut windows::Win32::Media::Audio::WAVEFORMATEX,
    endpoint_id: &EndpointId,
) -> Result<EndpointFormat, WindowsAudioError> {
    let raw = format_ptr as *const u8;
    let cb_size = std::ptr::read_unaligned(raw.add(16) as *const u16) as usize;
    if cb_size > 4096 {
        return Err(invalid_format(
            "GetMixFormat 返回的 cbSize 过大",
            Some(endpoint_id.0.clone()),
        ));
    }
    let length = 18usize + cb_size;
    let bytes = std::slice::from_raw_parts(raw, length);
    let format = decode_mix_format(bytes).map_err(|error| match error {
        WindowsAudioError::InvalidFormat { reason, .. } => {
            invalid_format(&reason, Some(endpoint_id.0.clone()))
        }
        other => other,
    })?;
    let format_tag = std::ptr::read_unaligned(raw as *const u16);
    let bits = std::ptr::read_unaligned(raw.add(14) as *const u16);
    let is_float = if format_tag == 3 {
        true
    } else if format_tag == 0xFFFE && cb_size >= 22 && length >= 40 {
        let sub_format = std::ptr::read_unaligned(raw.add(24) as *const windows::core::GUID);
        sub_format == windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
    } else {
        false
    };
    if bits != 32 || !is_float {
        return Err(WindowsAudioError::RenderFormatUnsupported {
            endpoint_id: endpoint_id.0.clone(),
            sample_rate: format.sample_rate,
            bits_per_sample: bits,
            channels: format.channels,
        });
    }
    if usize::from(format.channels) != INTERNAL_CHANNELS {
        return Err(WindowsAudioError::RenderFormatUnsupported {
            endpoint_id: endpoint_id.0.clone(),
            sample_rate: format.sample_rate,
            bits_per_sample: bits,
            channels: format.channels,
        });
    }
    Ok(format)
}

#[cfg(windows)]
fn write_render_block(
    sink: &mut WasapiRenderSink,
    samples: &[f32],
) -> Result<RenderWriteResult, WindowsAudioError> {
    if !samples.len().is_multiple_of(INTERNAL_CHANNELS) {
        return Err(WindowsAudioError::RenderInputUnaligned {
            samples: samples.len(),
            channels: INTERNAL_CHANNELS,
        });
    }
    let input_frames = samples.len() / INTERNAL_CHANNELS;
    if input_frames > sink.block_frames {
        return Err(WindowsAudioError::RenderBlockTooLarge {
            expected: sink.block_frames,
            actual: input_frames,
        });
    }

    let padding = unsafe { sink.client.GetCurrentPadding() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetCurrentPadding",
            Some(sink.endpoint_id.0.clone()),
            error,
        )
    })?;
    if padding > sink.buffer_frames {
        return Err(WindowsAudioError::RenderState {
            reason: "GetCurrentPadding 超过设备 buffer frame 数",
            endpoint_id: sink.endpoint_id.0.clone(),
        });
    }
    let available = sink.buffer_frames - padding;
    if available == 0 {
        return Ok(RenderWriteResult::NoSpace);
    }
    let (write_samples, frames_to_write) = if let Some(resampler) = sink.resampler.as_mut() {
        if input_frames != resampler.input_frames() {
            return Err(WindowsAudioError::RenderBlockTooLarge {
                expected: resampler.input_frames(),
                actual: input_frames,
            });
        }
        let expected_frames = resampler.output_frames_next() as u32;
        if available < expected_frames {
            return Ok(RenderWriteResult::NoSpace);
        }
        let written = resampler
            .process_interleaved(samples, &mut sink.resampled_output)
            .map_err(|error| WindowsAudioError::RenderResample {
                reason: error.to_string(),
                endpoint_id: sink.endpoint_id.0.clone(),
            })?;
        (
            &sink.resampled_output[..written * INTERNAL_CHANNELS],
            written as u32,
        )
    } else {
        let frames = input_frames as u32;
        if frames > available {
            return Ok(RenderWriteResult::NoSpace);
        }
        (samples, frames)
    };
    let frame_samples = frames_to_write as usize * INTERNAL_CHANNELS;
    let buffer = unsafe { sink.render_client.GetBuffer(frames_to_write) }.map_err(|error| {
        hresult_error(
            "IAudioRenderClient::GetBuffer",
            Some(sink.endpoint_id.0.clone()),
            error,
        )
    })?;
    if buffer.is_null() {
        return Err(WindowsAudioError::RenderState {
            reason: "IAudioRenderClient::GetBuffer 返回空指针",
            endpoint_id: sink.endpoint_id.0.clone(),
        });
    }
    let input_samples = write_samples.len().min(frame_samples);
    unsafe {
        std::ptr::copy_nonoverlapping(write_samples.as_ptr(), buffer as *mut f32, input_samples);
        if input_samples < frame_samples {
            std::ptr::write_bytes(
                (buffer as *mut f32).add(input_samples),
                0,
                frame_samples - input_samples,
            );
        }
    }
    unsafe { sink.render_client.ReleaseBuffer(frames_to_write, 0) }.map_err(|error| {
        hresult_error(
            "IAudioRenderClient::ReleaseBuffer",
            Some(sink.endpoint_id.0.clone()),
            error,
        )
    })?;
    Ok(RenderWriteResult::Written {
        frames: frames_to_write,
    })
}

#[cfg(windows)]
fn open_capture_source(endpoint_id: &EndpointId) -> Result<WasapiCaptureSource, WindowsAudioError> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eCapture, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, IMMEndpoint,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let com_result = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let com_hresult = com_result.0;
    if com_hresult < 0 && com_hresult as u32 != 0x80010106 {
        return Err(WindowsAudioError::ComInitialization {
            hresult: com_hresult,
        });
    }
    let com = ComGuard {
        should_uninitialize: com_hresult == 0 || com_hresult == 1,
    };
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|error| {
            hresult_error(
                "CoCreateInstance(MMDeviceEnumerator)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let wide_id: Vec<u16> = endpoint_id
        .0
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let device = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide_id.as_ptr())) }.map_err(
        |error| {
            hresult_error(
                "IMMDeviceEnumerator::GetDevice",
                Some(endpoint_id.0.clone()),
                error,
            )
        },
    )?;
    let state = unsafe { device.GetState() }.map_err(|error| {
        hresult_error("IMMDevice::GetState", Some(endpoint_id.0.clone()), error)
    })?;
    if state != DEVICE_STATE_ACTIVE {
        return Err(WindowsAudioError::CaptureState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    let endpoint = device.cast::<IMMEndpoint>().map_err(|error| {
        hresult_error(
            "IMMDevice::QueryInterface(IMMEndpoint)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    let flow = unsafe { endpoint.GetDataFlow() }.map_err(|error| {
        hresult_error(
            "IMMEndpoint::GetDataFlow",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if flow != eCapture {
        return Err(WindowsAudioError::CaptureState {
            reason: "指定 endpoint 不是 capture endpoint",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    let client: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|error| {
            hresult_error(
                "IMMDevice::Activate(IAudioClient)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let format_ptr = unsafe { client.GetMixFormat() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetMixFormat",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if format_ptr.is_null() {
        return Err(invalid_format(
            "IAudioClient::GetMixFormat 返回空指针",
            Some(endpoint_id.0.clone()),
        ));
    }
    let format_result = unsafe { inspect_capture_mix_format(format_ptr, endpoint_id) };
    let format = match format_result {
        Ok(format) => format,
        Err(error) => {
            unsafe {
                windows::Win32::System::Com::CoTaskMemFree(Some(
                    format_ptr as *const core::ffi::c_void,
                ))
            };
            return Err(error);
        }
    };
    let initialize_result =
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, format_ptr, None) };
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void))
    };
    initialize_result.map_err(|error| {
        hresult_error(
            "IAudioClient::Initialize(shared)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    let capture_client: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetService(IAudioCaptureClient)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    unsafe { client.Start() }.map_err(|error| {
        hresult_error("IAudioClient::Start", Some(endpoint_id.0.clone()), error)
    })?;
    Ok(WasapiCaptureSource {
        _com: com,
        client,
        capture_client,
        endpoint_id: endpoint_id.clone(),
        format,
    })
}

#[cfg(windows)]
fn open_device_loopback_source(
    endpoint_id: &EndpointId,
) -> Result<WasapiCaptureSource, WindowsAudioError> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, IMMEndpoint,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let com_result = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let com_hresult = com_result.0;
    if com_hresult < 0 && com_hresult as u32 != 0x80010106 {
        return Err(WindowsAudioError::ComInitialization {
            hresult: com_hresult,
        });
    }
    let com = ComGuard {
        should_uninitialize: com_hresult == 0 || com_hresult == 1,
    };
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|error| {
            hresult_error(
                "CoCreateInstance(MMDeviceEnumerator)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let wide_id: Vec<u16> = endpoint_id
        .0
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let device = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide_id.as_ptr())) }.map_err(
        |error| {
            hresult_error(
                "IMMDeviceEnumerator::GetDevice",
                Some(endpoint_id.0.clone()),
                error,
            )
        },
    )?;
    let state = unsafe { device.GetState() }.map_err(|error| {
        hresult_error("IMMDevice::GetState", Some(endpoint_id.0.clone()), error)
    })?;
    if state != DEVICE_STATE_ACTIVE {
        return Err(WindowsAudioError::CaptureState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    // Device Loopback 的目标是 render endpoint；确认流向为渲染，避免误用捕获设备。
    let endpoint = device.cast::<IMMEndpoint>().map_err(|error| {
        hresult_error(
            "IMMDevice::QueryInterface(IMMEndpoint)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    let flow = unsafe { endpoint.GetDataFlow() }.map_err(|error| {
        hresult_error(
            "IMMEndpoint::GetDataFlow",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if flow != eRender {
        return Err(WindowsAudioError::CaptureState {
            reason: "Device Loopback 目标必须是 render endpoint",
            endpoint_id: endpoint_id.0.clone(),
        });
    }
    let client: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|error| {
            hresult_error(
                "IMMDevice::Activate(IAudioClient)",
                Some(endpoint_id.0.clone()),
                error,
            )
        })?;
    let format_ptr = unsafe { client.GetMixFormat() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetMixFormat",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    if format_ptr.is_null() {
        return Err(invalid_format(
            "IAudioClient::GetMixFormat 返回空指针",
            Some(endpoint_id.0.clone()),
        ));
    }
    let format_result = unsafe { inspect_capture_mix_format(format_ptr, endpoint_id) };
    let format = match format_result {
        Ok(format) => format,
        Err(error) => {
            unsafe {
                windows::Win32::System::Com::CoTaskMemFree(Some(
                    format_ptr as *const core::ffi::c_void,
                ))
            };
            return Err(error);
        }
    };
    // Loopback 捕获共享模式渲染流：加 AUDCLNT_STREAMFLAGS_LOOPBACK，
    // 数据经 IAudioCaptureClient 读取，格式契约与普通 capture 相同（48k/32float/2ch）。
    let initialize_result = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            0,
            0,
            format_ptr,
            None,
        )
    };
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void))
    };
    initialize_result.map_err(|error| {
        hresult_error(
            "IAudioClient::Initialize(shared, loopback)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    let capture_client: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetService(IAudioCaptureClient)",
            Some(endpoint_id.0.clone()),
            error,
        )
    })?;
    unsafe { client.Start() }.map_err(|error| {
        hresult_error("IAudioClient::Start", Some(endpoint_id.0.clone()), error)
    })?;
    Ok(WasapiCaptureSource {
        _com: com,
        client,
        capture_client,
        endpoint_id: endpoint_id.clone(),
        format,
    })
}

#[cfg(windows)]
unsafe fn inspect_capture_mix_format(
    format_ptr: *mut windows::Win32::Media::Audio::WAVEFORMATEX,
    endpoint_id: &EndpointId,
) -> Result<EndpointFormat, WindowsAudioError> {
    let raw = format_ptr.cast::<u8>();
    let cb_size = std::ptr::read_unaligned(raw.add(16).cast::<u16>()) as usize;
    if cb_size > 4096 {
        return Err(invalid_format(
            "GetMixFormat 返回的 cbSize 过大",
            Some(endpoint_id.0.clone()),
        ));
    }
    let length = 18usize + cb_size;
    let bytes = std::slice::from_raw_parts(raw, length);
    let format = decode_mix_format(bytes).map_err(|error| match error {
        WindowsAudioError::InvalidFormat { reason, .. } => {
            invalid_format(&reason, Some(endpoint_id.0.clone()))
        }
        other => other,
    })?;
    let format_tag = std::ptr::read_unaligned(raw.cast::<u16>());
    let bits = std::ptr::read_unaligned(raw.add(14).cast::<u16>());
    let is_float = if format_tag == 3 {
        true
    } else if format_tag == 0xFFFE && cb_size >= 22 && length >= 40 {
        let sub_format = std::ptr::read_unaligned(raw.add(24).cast::<windows::core::GUID>());
        sub_format == windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
    } else {
        false
    };
    // 采样率不限：非 48 kHz 设备（如 44.1 kHz 虚拟声卡）由 capture worker
    // 的 FixedOutputResampler 重采样到内部 48 kHz（阶段 B.5）。
    if format.channels != loopmaster_audio_core::INTERNAL_CHANNELS as u16 || bits != 32 || !is_float
    {
        return Err(WindowsAudioError::CaptureFormatUnsupported {
            endpoint_id: endpoint_id.0.clone(),
            sample_rate: format.sample_rate,
            bits_per_sample: bits,
            channels: format.channels,
        });
    }
    Ok(format)
}

#[cfg(windows)]
fn drain_capture_packets<F>(
    source: &mut WasapiCaptureSource,
    on_packet: &mut F,
) -> Result<CaptureDrainResult, WindowsAudioError>
where
    F: FnMut(CapturePacket, Option<&[f32]>),
{
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    };

    let mut result = CaptureDrainResult::default();
    loop {
        let packet_frames =
            unsafe { source.capture_client.GetNextPacketSize() }.map_err(|error| {
                hresult_error(
                    "IAudioCaptureClient::GetNextPacketSize",
                    Some(source.endpoint_id.0.clone()),
                    error,
                )
            })?;
        if packet_frames == 0 {
            return Ok(result);
        }
        let mut data = std::ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        unsafe {
            source
                .capture_client
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
        }
        .map_err(|error| {
            hresult_error(
                "IAudioCaptureClient::GetBuffer",
                Some(source.endpoint_id.0.clone()),
                error,
            )
        })?;
        if frames == 0 {
            unsafe { source.capture_client.ReleaseBuffer(0) }.map_err(|error| {
                hresult_error(
                    "IAudioCaptureClient::ReleaseBuffer",
                    Some(source.endpoint_id.0.clone()),
                    error,
                )
            })?;
            return Err(WindowsAudioError::CaptureState {
                reason: "GetBuffer 返回 0 frame",
                endpoint_id: source.endpoint_id.0.clone(),
            });
        }
        let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        let discontinuity = (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;
        let timestamp_error = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0;
        let packet = CapturePacket {
            frames,
            silent,
            discontinuity,
            timestamp_error,
        };
        if silent {
            on_packet(packet, None);
        } else if data.is_null() {
            let release = unsafe { source.capture_client.ReleaseBuffer(frames) };
            let _ = release;
            return Err(WindowsAudioError::CaptureState {
                reason: "非静音 packet 的 GetBuffer 返回空指针",
                endpoint_id: source.endpoint_id.0.clone(),
            });
        } else {
            let sample_count = frames as usize * usize::from(source.format.channels);
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), sample_count) };
            on_packet(packet, Some(samples));
        }
        unsafe { source.capture_client.ReleaseBuffer(frames) }.map_err(|error| {
            hresult_error(
                "IAudioCaptureClient::ReleaseBuffer",
                Some(source.endpoint_id.0.clone()),
                error,
            )
        })?;
        result.packets += 1;
        result.frames += u64::from(frames);
        result.silent_packets += u64::from(silent);
        result.discontinuities += u64::from(discontinuity);
        result.timestamp_errors += u64::from(timestamp_error);
    }
}

#[cfg(windows)]
struct ComGuard {
    should_uninitialize: bool,
}
#[cfg(windows)]
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

#[cfg(windows)]
fn hresult_error(
    operation: &'static str,
    endpoint_id: Option<String>,
    error: windows::core::Error,
) -> WindowsAudioError {
    WindowsAudioError::HResult {
        operation,
        hresult: error.code().0,
        endpoint_id,
    }
}

#[cfg(windows)]
fn is_device_not_ready_hresult(hresult: i32) -> bool {
    matches!(
        hresult,
        HRESULT_ERROR_NOT_FOUND
            | HRESULT_ERROR_DEVICE_NOT_CONNECTED
            | AUDCLNT_E_DEVICE_INVALIDATED
            | AUDCLNT_E_SERVICE_NOT_RUNNING
            | AUDCLNT_E_SERVICE_NOT_RUNNING_LEGACY
            | AUDCLNT_E_ENDPOINT_CREATE_FAILED
    )
}

#[cfg(windows)]
unsafe fn get_endpoint_id(
    device: &windows::Win32::Media::Audio::IMMDevice,
) -> Result<String, WindowsAudioError> {
    // IMMDevice::GetId 返回由 COM 任务分配器分配的 PWSTR；复制为 Rust String
    // 后必须用 CoTaskMemFree 释放，不能用 Rust allocator 或直接持有该指针。
    let raw_id = device
        .GetId()
        .map_err(|e| hresult_error("IMMDevice::GetId", None, e))?;
    if raw_id.0.is_null() {
        return Err(invalid_format("IMMDevice::GetId 返回空指针", None));
    }
    let id_result = Ok(windows::core::PCWSTR(raw_id.0).display().to_string());
    windows::Win32::System::Com::CoTaskMemFree(Some(raw_id.0 as *const core::ffi::c_void));
    id_result
}

#[cfg(windows)]
unsafe fn get_endpoint_name(
    device: &windows::Win32::Media::Audio::IMMDevice,
    endpoint_id: &str,
) -> Result<String, WindowsAudioError> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;
    let store = device.OpenPropertyStore(STGM_READ).map_err(|e| {
        hresult_error(
            "IMMDevice::OpenPropertyStore",
            Some(endpoint_id.to_owned()),
            e,
        )
    })?;
    let mut value = store
        .GetValue(&PKEY_Device_FriendlyName as *const _)
        .map_err(|e| {
            hresult_error(
                "IPropertyStore::GetValue(PKEY_Device_FriendlyName)",
                Some(endpoint_id.to_owned()),
                e,
            )
        })?;
    // IPropertyStore::GetValue 返回拥有内部分配字符串的 PROPVARIANT；读取
    // pwszVal 后必须调用 PropVariantClear 释放其内部资源。
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::System::Variant::VT_LPWSTR;
    let name = if unsafe { value.Anonymous.Anonymous.vt } == VT_LPWSTR {
        windows::core::PWSTR(unsafe { value.Anonymous.Anonymous.Anonymous.pwszVal.0 })
            .display()
            .to_string()
    } else {
        String::new()
    };
    PropVariantClear(&mut value as *mut _).map_err(|e| {
        hresult_error(
            "PropVariantClear(PKEY_Device_FriendlyName)",
            Some(endpoint_id.to_owned()),
            e,
        )
    })?;
    Ok(if name.is_empty() {
        endpoint_id.to_owned()
    } else {
        name
    })
}

#[cfg(windows)]
unsafe fn get_endpoint_format(
    device: &windows::Win32::Media::Audio::IMMDevice,
    endpoint_id: &str,
) -> Result<EndpointFormat, WindowsAudioError> {
    use windows::Win32::Media::Audio::IAudioClient;
    use windows::Win32::System::Com::CLSCTX_ALL;
    let client = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .map_err(|e| {
            hresult_error(
                "IMMDevice::Activate(IAudioClient)",
                Some(endpoint_id.to_owned()),
                e,
            )
        })?;
    let format_ptr = client.GetMixFormat().map_err(|e| {
        hresult_error(
            "IAudioClient::GetMixFormat",
            Some(endpoint_id.to_owned()),
            e,
        )
    })?;
    if format_ptr.is_null() {
        return Err(invalid_format(
            "IAudioClient::GetMixFormat 返回空指针",
            Some(endpoint_id.to_owned()),
        ));
    }
    // GetMixFormat 返回 CoTaskMemAlloc 分配的 WAVEFORMATEX 指针。先读取固定
    // 头部的 cbSize，再构造有界切片，最后始终用 CoTaskMemFree 释放返回缓冲区。
    let cb_size =
        std::ptr::read_unaligned((format_ptr as *const u8).add(16) as *const u16) as usize;
    if cb_size > 4096 {
        windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void));
        return Err(invalid_format(
            "GetMixFormat 返回的 cbSize 过大",
            Some(endpoint_id.to_owned()),
        ));
    }
    let bytes = std::slice::from_raw_parts(format_ptr as *const u8, 18 + cb_size);
    let result = decode_mix_format(bytes).map_err(|e| match e {
        WindowsAudioError::InvalidFormat { reason, .. } => {
            invalid_format(&reason, Some(endpoint_id.to_owned()))
        }
        other => other,
    });
    windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_flow_labels_are_stable() {
        assert_eq!(EndpointFlow::Capture.as_str(), "capture");
        assert_eq!(EndpointFlow::Render.display_name(), "Render");
    }

    #[test]
    fn process_info_model_preserves_pid_and_optional_path() {
        let process = ProcessInfo {
            pid: 1234,
            name: "player.exe".to_owned(),
            executable_path: Some(r"C:\Player\player.exe".to_owned()),
        };
        assert_eq!(process.pid, 1234);
        assert_eq!(process.name, "player.exe");
        assert!(process.executable_path.is_some());
    }
    #[test]
    fn decodes_waveformat_extensible() {
        let mut raw = vec![0u8; 40];
        raw[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
        raw[2..4].copy_from_slice(&2u16.to_le_bytes());
        raw[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        raw[14..16].copy_from_slice(&32u16.to_le_bytes());
        raw[16..18].copy_from_slice(&22u16.to_le_bytes());
        raw[20..24].copy_from_slice(&0x3u32.to_le_bytes());
        let f = decode_mix_format(&raw).unwrap();
        assert_eq!(
            (f.sample_rate, f.bits_per_sample, f.channels, f.channel_mask),
            (48_000, 32, 2, 3)
        );
        // subformat 字节未被写入，不是 IEEE float。
        assert!(!f.is_float);
    }

    #[test]
    fn identifies_ieee_float_extensible_subformat() {
        let mut raw = vec![0u8; 40];
        raw[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
        raw[2..4].copy_from_slice(&2u16.to_le_bytes());
        raw[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        raw[14..16].copy_from_slice(&32u16.to_le_bytes());
        raw[16..18].copy_from_slice(&22u16.to_le_bytes());
        raw[20..24].copy_from_slice(&0x3u32.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: 00000003-0000-0010-8000-00AA00389B71
        raw[24..40].copy_from_slice(&[
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
        let f = decode_mix_format(&raw).unwrap();
        assert!(f.is_float);
        assert!(f.capture_compatible());
        assert!(f.render_compatible());
    }

    #[test]
    fn identifies_plain_ieee_float_tag() {
        let mut raw = vec![0u8; 18];
        raw[0..2].copy_from_slice(&3u16.to_le_bytes());
        raw[2..4].copy_from_slice(&2u16.to_le_bytes());
        raw[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        raw[14..16].copy_from_slice(&32u16.to_le_bytes());
        let f = decode_mix_format(&raw).unwrap();
        assert!(f.is_float);
        assert!(f.capture_compatible());
    }

    #[test]
    fn distinguishes_capture_and_render_contracts() {
        // 44.1 kHz、32-bit float、双声道：可作 render（重采样），不可作 capture。
        let render_only = EndpointFormat {
            sample_rate: 44_100,
            bits_per_sample: 32,
            channels: 2,
            channel_mask: 0x3,
            is_float: true,
        };
        assert!(render_only.render_compatible());
        assert!(!render_only.capture_compatible());

        // 48 kHz、16-bit 整数、双声道：既不满足 capture 也不满足 render。
        let pcm_16 = EndpointFormat {
            sample_rate: 48_000,
            bits_per_sample: 16,
            channels: 2,
            channel_mask: 0x3,
            is_float: false,
        };
        assert!(!pcm_16.capture_compatible());
        assert!(!pcm_16.render_compatible());
    }
    #[test]
    fn rejects_truncated_mix_format() {
        let e = decode_mix_format(&[0u8; 17]).unwrap_err();
        assert!(matches!(e, WindowsAudioError::InvalidFormat { .. }));
    }
    #[test]
    fn preserves_hresult_and_endpoint_id() {
        let e = WindowsAudioError::HResult {
            operation: "test",
            hresult: -2,
            endpoint_id: Some("endpoint-id".into()),
        };
        assert_eq!(e.hresult(), Some(-2));
        assert_eq!(e.endpoint_id(), Some("endpoint-id"));
    }

    #[test]
    fn classifies_device_hresult_as_recoverable() {
        for hresult in [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_SERVICE_NOT_RUNNING_LEGACY,
            AUDCLNT_E_ENDPOINT_CREATE_FAILED,
            HRESULT_ERROR_DEVICE_NOT_CONNECTED,
            HRESULT_ERROR_NOT_FOUND,
        ] {
            let error = WindowsAudioError::HResult {
                operation: "test",
                hresult,
                endpoint_id: Some("endpoint-id".into()),
            };
            assert_eq!(
                error.failure_kind(),
                WindowsAudioFailureKind::DeviceUnavailable,
                "HRESULT=0x{hresult:08X}"
            );
            assert!(error.is_device_failure());
        }
    }

    #[test]
    fn does_not_classify_unrelated_hresult_as_device_failure() {
        let error = WindowsAudioError::HResult {
            operation: "test",
            hresult: -2,
            endpoint_id: None,
        };
        assert_eq!(error.failure_kind(), WindowsAudioFailureKind::Other);
        assert!(!error.is_device_failure());
    }

    #[test]
    fn classifies_inactive_endpoint_state_as_device_failure() {
        let capture = WindowsAudioError::CaptureState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: "capture-id".into(),
        };
        let render = WindowsAudioError::RenderState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: "render-id".into(),
        };
        assert_eq!(
            capture.failure_kind(),
            WindowsAudioFailureKind::DeviceUnavailable
        );
        assert_eq!(
            render.failure_kind(),
            WindowsAudioFailureKind::DeviceUnavailable
        );
    }

    #[test]
    fn keeps_other_capture_state_errors_non_recoverable() {
        let error = WindowsAudioError::CaptureState {
            reason: "GetBuffer 返回 0 frame",
            endpoint_id: "capture-id".into(),
        };
        assert_eq!(error.failure_kind(), WindowsAudioFailureKind::Other);
    }

    #[test]
    fn render_errors_expose_endpoint_context() {
        let error = WindowsAudioError::RenderFormatUnsupported {
            endpoint_id: "render-id".to_owned(),
            sample_rate: 48_000,
            bits_per_sample: 16,
            channels: 2,
        };
        assert_eq!(error.hresult(), None);
        assert_eq!(error.endpoint_id(), Some("render-id"));
        assert!(error.to_string().contains("32-bit IEEE float"));
    }

    #[test]
    fn capture_errors_expose_endpoint_context() {
        let error = WindowsAudioError::CaptureState {
            reason: "test",
            endpoint_id: "capture-id".to_owned(),
        };
        assert_eq!(error.hresult(), None);
        assert_eq!(error.endpoint_id(), Some("capture-id"));
    }

    #[cfg(not(windows))]
    #[test]
    fn backend_is_explicitly_unsupported_off_windows() {
        assert!(matches!(
            WindowsAudioBackend::new(),
            Err(WindowsAudioError::UnsupportedPlatform)
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn render_open_is_explicitly_unsupported_off_windows() {
        let backend = WindowsAudioBackend::new();
        assert!(matches!(
            backend,
            Err(WindowsAudioError::UnsupportedPlatform)
        ));
    }
}
