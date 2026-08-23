//! Windows 音频平台适配层。
//!
//! Windows 音频平台适配层和正式音频引擎运行时。

use loopmaster_audio_core::{
    AudioFormat, EndpointId, FixedInputResampler, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE,
};
use thiserror::Error;

mod runtime;

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
}

impl EndpointFormat {
    /// 将 Windows 格式摘要转换为平台无关的格式模型。
    pub const fn audio_format(self) -> AudioFormat {
        AudioFormat {
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
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
}

impl EndpointInfo {
    /// 在格式字段完整时返回一个便于输出的格式摘要。
    pub fn endpoint_format(&self) -> Option<EndpointFormat> {
        match (self.format, self.bits_per_sample, self.channel_mask) {
            (Some(format), Some(bits_per_sample), Some(channel_mask)) => Some(EndpointFormat {
                sample_rate: format.sample_rate,
                bits_per_sample,
                channels: format.channels,
                channel_mask,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
/// Windows/WASAPI 操作错误，保留 HRESULT 和相关 endpoint ID。
pub enum WindowsAudioError {
    #[error("Windows 音频 endpoint 仅支持 Windows 平台")]
    UnsupportedPlatform,
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
    #[error("render endpoint 格式不满足 MVP 要求（48 kHz、32-bit IEEE float、2 声道）: endpoint={endpoint_id}, {sample_rate} Hz, {bits_per_sample} bit, {channels} channels")]
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
        match self.hresult() {
            Some(
                AUDCLNT_E_DEVICE_INVALIDATED
                | AUDCLNT_E_SERVICE_NOT_RUNNING
                | AUDCLNT_E_SERVICE_NOT_RUNNING_LEGACY
                | AUDCLNT_E_ENDPOINT_CREATE_FAILED
                | HRESULT_ERROR_DEVICE_NOT_CONNECTED,
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
    Ok(EndpointFormat {
        sample_rate,
        bits_per_sample,
        channels,
        channel_mask,
    })
}

fn invalid_format(reason: &str, endpoint_id: Option<String>) -> WindowsAudioError {
    WindowsAudioError::InvalidFormat {
        reason: reason.to_owned(),
        endpoint_id,
    }
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
                    });
                }
            }
            Ok(endpoints)
        }
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
    if format.sample_rate != loopmaster_audio_core::INTERNAL_SAMPLE_RATE
        || format.channels != loopmaster_audio_core::INTERNAL_CHANNELS as u16
        || bits != 32
        || !is_float
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
