//! Windows 音频平台适配层。
//!
//! 当前模块只负责读取 WASAPI endpoint 的静态能力信息。这里不打开音频流，
//! 也不实现捕获、渲染、混音或设备恢复；这些能力由后续阶段单独实现。

use loopmaster_audio_core::{AudioFormat, EndpointId};
use thiserror::Error;

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
            _ => None,
        }
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

    #[cfg(not(windows))]
    #[test]
    fn backend_is_explicitly_unsupported_off_windows() {
        assert!(matches!(
            WindowsAudioBackend::new(),
            Err(WindowsAudioError::UnsupportedPlatform)
        ));
    }
}
