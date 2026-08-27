//! 从进程可执行文件路径提取应用图标，编码为 PNG 的 base64 data URI，供前端在
//! Process Loopback 列表里展示（类似 macOS Loopback 的「Running Applications」图标）。
//!
//! 仅 Windows 有效；非 Windows 平台一律返回 `None`。图标提取走 GDI+
//! （`GdipCreateBitmapFromHICON` + `GdipSaveImageToStream`），避免引入额外的 PNG
//! 编码依赖。

use std::ptr;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HGLOBAL, MAX_PATH};
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromHICON, GdipDisposeImage, GdipSaveImageToStream, GdiplusShutdown,
    GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, GpBitmap, GpImage, Status,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::UI::Shell::{
    ExtractIconExW, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SHELLICONSIZE,
    SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

/// PNG 编码器 CLSID：{557CF406-1A04-11D3-9A73-0000F81EF32E}
const PNG_ENCODER_CLSID: windows::core::GUID =
    windows::core::GUID::from_u128(0x557CF406_1A04_11D3_9A73_0000F81EF32E);

#[cfg(windows)]
struct GdipImageGuard(*mut GpImage);

#[cfg(windows)]
impl Drop for GdipImageGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GdipDisposeImage(self.0) };
        }
    }
}

/// 返回可执行文件图标的 PNG data URI（`data:image/png;base64,...`），失败返回 `None`。
pub fn process_icon_data_uri(executable_path: &str) -> Option<String> {
    extract_icon_base64(executable_path)
}

#[cfg(not(windows))]
fn extract_icon_base64(_path: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn extract_icon_base64(path: &str) -> Option<String> {
    // 优先从可执行文件本身取第一个图标，失败再回退到 SHGetFileInfo（按扩展名取文件类型图标）。
    let hicon = extract_first_icon_from_exe(path).or_else(|| extract_shell_icon(path))?;

    let mut gdiplus_token: usize = 0;
    let startup_input = GdiplusStartupInput {
        GdiplusVersion: 1,
        DebugEventCallback: 0,
        SuppressBackgroundThread: false.into(),
        SuppressExternalCodecs: false.into(),
    };
    let startup = unsafe {
        GdiplusStartup(
            &mut gdiplus_token,
            &startup_input,
            ptr::null_mut::<GdiplusStartupOutput>(),
        )
    };
    if startup != Status(0) {
        let _ = unsafe { DestroyIcon(hicon) };
        return None;
    }

    let result = (|| -> Option<String> {
        let mut bitmap: *mut GpBitmap = ptr::null_mut();
        let status = unsafe { GdipCreateBitmapFromHICON(hicon, &mut bitmap) };
        if status != Status(0) || bitmap.is_null() {
            return None;
        }
        let image = GdipImageGuard(bitmap.cast());
        // delete_on_release=true：IStream 释放时同时释放内部 HGLOBAL。
        let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }.ok()?;
        let status =
            unsafe { GdipSaveImageToStream(image.0, &stream, &PNG_ENCODER_CLSID, ptr::null()) };
        if status != Status(0) {
            return None;
        }
        let bytes = read_stream_to_end(&stream)?;
        if bytes.is_empty() {
            return None;
        }
        let b64 = base64_encode(&bytes);
        Some(format!("data:image/png;base64,{b64}"))
    })();

    unsafe {
        GdiplusShutdown(gdiplus_token);
        let _ = DestroyIcon(hicon);
    }

    result
}

#[cfg(windows)]
fn extract_first_icon_from_exe(
    path: &str,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::UI::WindowsAndMessaging::HICON;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut large: [HICON; 1] = [HICON(ptr::null_mut())];
    let mut small: [HICON; 1] = [HICON(ptr::null_mut())];
    let count = unsafe {
        ExtractIconExW(
            PCWSTR(wide.as_ptr()),
            0,
            Some(large.as_mut_ptr()),
            Some(small.as_mut_ptr()),
            1,
        )
    };
    // large 优先，否则 small；都取不到时放弃。
    if count > 0 && !large[0].is_invalid() {
        let _ = unsafe { DestroyIcon(small[0]) };
        Some(large[0])
    } else if count > 0 && !small[0].is_invalid() {
        Some(small[0])
    } else {
        None
    }
}

#[cfg(windows)]
fn extract_shell_icon(path: &str) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW {
        hIcon: Default::default(),
        iIcon: 0,
        dwAttributes: 0,
        szDisplayName: [0; MAX_PATH as usize],
        szTypeName: [0; 80],
    };
    let hr = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            u32::try_from(std::mem::size_of::<SHFILEINFOW>())
                .expect("SHFILEINFOW 大小应可表示为 u32"),
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_SHELLICONSIZE | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if hr == 0 || info.hIcon.is_invalid() {
        None
    } else {
        Some(info.hIcon)
    }
}

#[cfg(windows)]
fn read_stream_to_end(stream: &windows::Win32::System::Com::IStream) -> Option<Vec<u8>> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;

    // GdipSaveImageToStream 写完后流指针在末尾，必须 seek 到开头再读。
    let mut new_pos: u64 = 0;
    unsafe {
        stream
            .Seek(0, STREAM_SEEK_SET, Some(&mut new_pos as *mut u64))
            .ok()?;
    }

    let mut out = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let mut read: u32 = 0;
        let hr = unsafe {
            stream.Read(
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                Some(&mut read),
            )
        };
        if hr.is_err() {
            return None;
        }
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..read as usize]);
    }
    Some(out)
}

#[cfg(windows)]
fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
