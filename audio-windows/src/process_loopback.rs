//! Windows 10 2004+ Process Loopback 捕获适配器。
//!
//! 该模块只负责把指定进程树的系统播放混音暴露成 capture source；它不创建
//! endpoint，也不改变系统默认设备。Process Loopback 通过系统的虚拟 endpoint
//! `VAD\\Process_Loopback` 激活，最终仍使用普通 `IAudioClient` 捕获接口。

use super::{
    hresult_error, invalid_format, CaptureDrainResult, CapturePacket, EndpointFormat,
    WindowsAudioError,
};

/// 捕获指定进程树音频的 WASAPI source。
///
/// `pid` 对应目标进程。Windows Process Loopback 的默认模式会包含该进程的
/// 子进程；如需排除目标进程树，应在后续 API 中显式暴露模式选择。当前 MVP
/// 固定使用 include 模式，并要求系统返回 48 kHz、32-bit IEEE float、双声道。
pub struct ProcessLoopbackSource {
    pid: u32,
    endpoint_id: String,
    format: EndpointFormat,
    #[cfg(windows)]
    _com: ComGuard,
    #[cfg(windows)]
    client: windows::Win32::Media::Audio::IAudioClient,
    #[cfg(windows)]
    capture_client: windows::Win32::Media::Audio::IAudioCaptureClient,
}

impl ProcessLoopbackSource {
    /// 打开目标进程树的 Process Loopback 捕获。
    pub fn open(pid: u32) -> Result<Self, WindowsAudioError> {
        if pid == 0 {
            return Err(WindowsAudioError::ProcessLoopbackInvalidPid { pid });
        }
        #[cfg(not(windows))]
        {
            let _ = pid;
            Err(WindowsAudioError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            open_windows(pid)
        }
    }

    /// 目标进程 ID。
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Process Loopback 返回的固定格式摘要。
    pub const fn format(&self) -> EndpointFormat {
        self.format
    }

    /// 以 `CapturePacket` 回调排空当前可用 packet。
    ///
    /// 非静音 packet 的样本仅在回调期间有效；回调返回后 WASAPI 会立即释放
    /// packet，调用方不得保存该切片。
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
            drain_windows(self, &mut on_packet)
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
fn open_windows(pid: u32) -> Result<ProcessLoopbackSource, WindowsAudioError> {
    use std::sync::{Arc, Mutex};
    use windows::core::Interface;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Media::Audio::{
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IAudioCaptureClient, IAudioClient,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    let com_hresult = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.0;
    if com_hresult < 0 && com_hresult as u32 != 0x80010106 {
        return Err(WindowsAudioError::ComInitialization {
            hresult: com_hresult,
        });
    }
    let com = ComGuard {
        should_uninitialize: com_hresult == 0 || com_hresult == 1,
    };
    let endpoint_id = format!("process:{pid}");

    let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|error| {
        hresult_error(
            "CreateEventW(ProcessLoopback)",
            Some(endpoint_id.clone()),
            error,
        )
    })?;
    let event = Arc::new(HandleGuard(event));
    let shared = Arc::new(Mutex::new(ActivationResult::default()));

    let payload = Arc::new(ActivationPayload::new(pid));

    #[windows_core::implement(IActivateAudioInterfaceCompletionHandler)]
    struct CompletionHandler {
        event: Arc<HandleGuard>,
        result: Arc<Mutex<ActivationResult>>,
        // ActivateAudioInterfaceAsync may retain PROPVARIANT until its callback.
        // Keep both the blob and its pointed-to params alive across that boundary.
        _payload: Arc<ActivationPayload>,
    }
    impl windows::Win32::Media::Audio::IActivateAudioInterfaceCompletionHandler_Impl
        for CompletionHandler_Impl
    {
        fn ActivateCompleted(
            &self,
            operation: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let mut result = self
                .result
                .lock()
                .expect("activation result mutex poisoned");
            let callback_result = (|| {
                let operation = operation.ok()?;
                let mut activate_hresult = windows::core::HRESULT(0);
                let mut activated: Option<windows::core::IUnknown> = None;
                unsafe { operation.GetActivateResult(&mut activate_hresult, &mut activated)? };
                result.hresult = activate_hresult.0;
                if activate_hresult.0 >= 0 {
                    result.client = activated
                        .and_then(|unknown| unknown.cast::<IAudioClient>().ok())
                        .and_then(|client| windows_core::AgileReference::new(&client).ok());
                    if result.client.is_none() {
                        result.hresult = 0x80004005u32 as i32;
                    }
                }
                Ok(())
            })();
            // Always wake the waiting thread, including GetActivateResult failures.
            let signal_result =
                unsafe { windows::Win32::System::Threading::SetEvent(self.event.0) };
            callback_result.and(signal_result)
        }
    }

    let handler_object = windows::core::ComObject::new(CompletionHandler {
        event: Arc::clone(&event),
        result: Arc::clone(&shared),
        _payload: Arc::clone(&payload),
    });
    let handler: IActivateAudioInterfaceCompletionHandler = handler_object.to_interface();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&payload.blob as *const PROPVARIANT),
            &handler,
        )
    }
    .map_err(|error| {
        hresult_error(
            "ActivateAudioInterfaceAsync(ProcessLoopback)",
            Some(endpoint_id.clone()),
            error,
        )
    })?;
    let wait = unsafe { WaitForSingleObject(event.0, 30_000) };
    if wait != WAIT_OBJECT_0 {
        return Err(WindowsAudioError::HResult {
            operation: "ActivateAudioInterfaceAsync(ProcessLoopback) 等待",
            hresult: 0x800705B4u32 as i32,
            endpoint_id: Some(endpoint_id),
        });
    }
    let mut result = shared.lock().expect("activation result mutex poisoned");
    if result.hresult < 0 {
        return Err(WindowsAudioError::HResult {
            operation: "IActivateAudioInterfaceAsyncOperation::GetActivateResult",
            hresult: result.hresult,
            endpoint_id: Some(endpoint_id),
        });
    }
    let client = result
        .client
        .take()
        .ok_or_else(|| WindowsAudioError::HResult {
            operation: "IActivateAudioInterfaceAsyncOperation::GetActivateResult",
            hresult: 0x80004005u32 as i32,
            endpoint_id: Some(endpoint_id.clone()),
        })?;
    drop(result);
    let client = client.resolve().map_err(|error| {
        hresult_error(
            "AgileReference::resolve(ProcessLoopback)",
            Some(endpoint_id.clone()),
            error,
        )
    })?;
    drop(operation);
    drop(handler);
    drop(handler_object);

    let format_ptr = unsafe { client.GetMixFormat() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetMixFormat(ProcessLoopback)",
            Some(endpoint_id.clone()),
            error,
        )
    })?;
    if format_ptr.is_null() {
        return Err(invalid_format(
            "Process Loopback GetMixFormat 返回空指针",
            Some(endpoint_id),
        ));
    }
    let format_result = unsafe {
        super::inspect_capture_mix_format(format_ptr, &super::EndpointId(endpoint_id.clone()))
    };
    let format = match format_result {
        Ok(format) => format,
        Err(error) => {
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const _)) };
            return Err(error);
        }
    };
    let initialize = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            format_ptr,
            None,
        )
    };
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr as *const _)) };
    initialize.map_err(|error| {
        hresult_error(
            "IAudioClient::Initialize(ProcessLoopback)",
            Some(format!("process:{pid}")),
            error,
        )
    })?;
    let capture_client: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|error| {
        hresult_error(
            "IAudioClient::GetService(IAudioCaptureClient, ProcessLoopback)",
            Some(format!("process:{pid}")),
            error,
        )
    })?;
    unsafe { client.Start() }.map_err(|error| {
        hresult_error(
            "IAudioClient::Start(ProcessLoopback)",
            Some(format!("process:{pid}")),
            error,
        )
    })?;

    Ok(ProcessLoopbackSource {
        pid,
        endpoint_id: format!("process:{pid}"),
        format,
        _com: com,
        client,
        capture_client,
    })
}

#[cfg(windows)]
struct ActivationPayload {
    _params: Box<windows::Win32::Media::Audio::AUDIOCLIENT_ACTIVATION_PARAMS>,
    blob: windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
}

#[cfg(windows)]
// The payload is immutable after construction. Its boxed params are kept at a
// stable address until the async completion handler is released.
unsafe impl Send for ActivationPayload {}
#[cfg(windows)]
unsafe impl Sync for ActivationPayload {}

#[cfg(windows)]
impl ActivationPayload {
    fn new(pid: u32) -> Self {
        use std::mem::{size_of, ManuallyDrop};
        use windows::Win32::Media::Audio::{
            AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        };
        use windows::Win32::System::Com::StructuredStorage::{
            PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
        };
        use windows::Win32::System::Com::BLOB;
        use windows::Win32::System::Variant::VT_BLOB;

        let params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        });
        let blob = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: params.as_ref() as *const _ as *mut _,
                        },
                    },
                }),
            },
        };
        Self {
            _params: params,
            blob,
        }
    }
}

#[cfg(windows)]
#[derive(Default)]
struct ActivationResult {
    hresult: i32,
    client: Option<windows_core::AgileReference<windows::Win32::Media::Audio::IAudioClient>>,
}

#[cfg(windows)]
struct HandleGuard(windows::Win32::Foundation::HANDLE);

// A kernel event handle is process-wide and may be waited/signaled by the
// activation callback thread. Arc controls the single CloseHandle operation.
#[cfg(windows)]
unsafe impl Send for HandleGuard {}
#[cfg(windows)]
unsafe impl Sync for HandleGuard {}

#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
impl Drop for ProcessLoopbackSource {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(windows)]
fn drain_windows<F>(
    source: &mut ProcessLoopbackSource,
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
                    "IAudioCaptureClient::GetNextPacketSize(ProcessLoopback)",
                    Some(source.endpoint_id.clone()),
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
                "IAudioCaptureClient::GetBuffer(ProcessLoopback)",
                Some(source.endpoint_id.clone()),
                error,
            )
        })?;
        if frames == 0 {
            let _ = unsafe { source.capture_client.ReleaseBuffer(0) };
            return Err(WindowsAudioError::CaptureState {
                reason: "Process Loopback GetBuffer 返回 0 frame",
                endpoint_id: source.endpoint_id.clone(),
            });
        }
        let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        let packet = CapturePacket {
            frames,
            silent,
            discontinuity: (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0,
            timestamp_error: (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0,
        };
        if silent {
            on_packet(packet, None);
        } else if data.is_null() {
            let _ = unsafe { source.capture_client.ReleaseBuffer(frames) };
            return Err(WindowsAudioError::CaptureState {
                reason: "Process Loopback 非静音 packet 返回空指针",
                endpoint_id: source.endpoint_id.clone(),
            });
        } else {
            let samples = unsafe {
                std::slice::from_raw_parts(
                    data.cast::<f32>(),
                    frames as usize * source.format.channels as usize,
                )
            };
            on_packet(packet, Some(samples));
        }
        unsafe { source.capture_client.ReleaseBuffer(frames) }.map_err(|error| {
            hresult_error(
                "IAudioCaptureClient::ReleaseBuffer(ProcessLoopback)",
                Some(source.endpoint_id.clone()),
                error,
            )
        })?;
        result.packets += 1;
        result.frames += u64::from(frames);
        result.silent_packets += u64::from(silent);
        result.discontinuities += u64::from(packet.discontinuity);
        result.timestamp_errors += u64::from(packet.timestamp_error);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn activation_blob_uses_windows_abi_sizes() {
        use std::mem::{align_of, size_of};
        use windows::Win32::Media::Audio::{
            AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        };
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows::Win32::System::Com::BLOB;
        assert_eq!(size_of::<AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS>(), 8);
        assert_eq!(align_of::<AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS>(), 4);
        assert_eq!(size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(), 12);
        assert_eq!(align_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(), 4);
        assert_eq!(size_of::<BLOB>(), 16);
        assert_eq!(size_of::<PROPVARIANT>(), 24);
    }

    #[test]
    fn zero_pid_is_rejected_before_platform_call() {
        let error = match super::ProcessLoopbackSource::open(0) {
            Ok(_) => panic!("zero pid must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            super::WindowsAudioError::ProcessLoopbackInvalidPid { pid: 0 }
        ));
    }
}
