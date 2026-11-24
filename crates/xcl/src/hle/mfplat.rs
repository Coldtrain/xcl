//! `mfplat.dll` – Media Foundation console helpers on top of Windows Media Foundation.
//!
//! All standard Media Foundation exports (`MFStartup`, `MFCreateDXGIDeviceManager`,
//! `MFCreateMediaType`, `MFCreateSample`, `MFCreateAttributes`, …) fall back transparently
//! to the system `mfplat.dll` (see `hle::build`).
//!
//! Xbox GDK titles additionally import `MFResetDXGIDeviceManagerX`, which binds a D3D12
//! device to an `IMFDXGIDeviceManager`. On Windows PC, `IMFDXGIDeviceManager::ResetDevice`
//! expects an `ID3D11Device` (typically with video + BGRA support and multithread
//! protection enabled) and rejects `ID3D12Device` directly. When a direct `ResetDevice`
//! fails for an `ID3D12Device`, we create a companion D3D11 device on the same adapter
//! LUID, enable `ID3D10Multithread::SetMultithreadProtected(TRUE)`, and pass that to
//! `IMFDXGIDeviceManager::ResetDevice` with the caller's `resetToken`.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::{GUID, IUnknown, Interface};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4};
use windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager;

use crate::util::{E_POINTER, HRESULT, S_OK};

const D3D_DRIVER_TYPE_UNKNOWN: i32 = 0;
const D3D_DRIVER_TYPE_HARDWARE: i32 = 1;
const D3D_DRIVER_TYPE_WARP: i32 = 5;

const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
const D3D11_CREATE_DEVICE_VIDEO_SUPPORT: u32 = 0x800;
const D3D11_SDK_VERSION: u32 = 7;

const D3D_FEATURE_LEVELS: [u32; 4] = [
    0xb100, // D3D_FEATURE_LEVEL_11_1
    0xb000, // D3D_FEATURE_LEVEL_11_0
    0xa100, // D3D_FEATURE_LEVEL_10_1
    0xa000, // D3D_FEATURE_LEVEL_10_0
];

/// `ID3D10Multithread`: `{9B7E4E00-342C-4106-A19F-4F2704F689F0}`.
const IID_ID3D10_MULTITHREAD: GUID = GUID::from_u128(0x9b7e4e00_342c_4106_a19f_4f2704f689f0);
const ID3D10_MULTITHREAD_SET_PROTECTED: usize = 5;

type D3D11CreateDeviceFn = unsafe extern "system" fn(
    pAdapter: *mut c_void,
    DriverType: i32,
    Software: *mut c_void,
    Flags: u32,
    pFeatureLevels: *const u32,
    FeatureLevels: u32,
    SDKVersion: u32,
    ppDevice: *mut *mut c_void,
    pFeatureLevel: *mut u32,
    ppImmediateContext: *mut *mut c_void,
) -> HRESULT;

type SetMultithreadProtectedFn = unsafe extern "system" fn(*mut c_void, i32) -> i32;

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![MFResetDXGIDeviceManagerX, MFCreateSourceResolver]
}

/// Enable `ID3D10Multithread::SetMultithreadProtected(TRUE)` on a D3D11 device as required
/// by `IMFDXGIDeviceManager::ResetDevice`.
unsafe fn enable_multithread_protection(d3d11_dev: &IUnknown) {
    let mut mt_raw: *mut c_void = std::ptr::null_mut();
    let hr = d3d11_dev.query(&IID_ID3D10_MULTITHREAD, &mut mt_raw);
    if hr.is_ok() && !mt_raw.is_null() {
        let mt = IUnknown::from_raw(mt_raw);
        let vtbl = *(mt.as_raw() as *const *const usize);
        let set_protected: SetMultithreadProtectedFn =
            std::mem::transmute(*vtbl.add(ID3D10_MULTITHREAD_SET_PROTECTED));
        set_protected(mt.as_raw(), 1);
    }
}

/// Find the DXGI adapter matching `luid` (if non-zero).
unsafe fn adapter_for_luid(luid: LUID) -> Option<IDXGIAdapter> {
    if luid.LowPart == 0 && luid.HighPart == 0 {
        return None;
    }
    let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
    factory.EnumAdapterByLuid::<IDXGIAdapter>(luid).ok()
}

/// Create a companion D3D11 device (with video + BGRA support and multithread protection)
/// on the given adapter LUID so Windows' `IMFDXGIDeviceManager` can drive hardware MFTs.
unsafe fn create_d3d11_bridge(luid: Option<LUID>) -> Result<IUnknown, String> {
    let d3d11 = crate::resolver::system_module("d3d11.dll")
        .ok_or_else(|| "d3d11.dll not available".to_string())?;
    let proc = crate::resolver::system_proc_in(
        d3d11,
        &crate::pe::ImportName::Name { hint: 0, name: "D3D11CreateDevice".into() },
    )
    .ok_or_else(|| "D3D11CreateDevice export missing".to_string())?;
    let create_device: D3D11CreateDeviceFn = std::mem::transmute(proc);

    let adapter = luid.and_then(|l| adapter_for_luid(l));

    let try_create = |adapter_ptr: *mut c_void, driver_type: i32, flags: u32| -> Result<IUnknown, HRESULT> {
        let mut dev: *mut c_void = std::ptr::null_mut();
        let mut ctx: *mut c_void = std::ptr::null_mut();
        let mut fl: u32 = 0;
        let hr = create_device(
            adapter_ptr,
            driver_type,
            std::ptr::null_mut(),
            flags,
            D3D_FEATURE_LEVELS.as_ptr(),
            D3D_FEATURE_LEVELS.len() as u32,
            D3D11_SDK_VERSION,
            &mut dev,
            &mut fl,
            &mut ctx,
        );
        if !ctx.is_null() {
            drop(IUnknown::from_raw(ctx));
        }
        if hr >= 0 && !dev.is_null() {
            let unk = IUnknown::from_raw(dev);
            enable_multithread_protection(&unk);
            Ok(unk)
        } else {
            if !dev.is_null() {
                drop(IUnknown::from_raw(dev));
            }
            Err(hr)
        }
    };

    let mut last_hr: HRESULT = 0;
    let flag_sets = [
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ];

    // 1. Prefer the exact adapter matching the D3D12 device's LUID.
    if let Some(ref a) = adapter {
        for flags in flag_sets {
            match try_create(a.as_raw(), D3D_DRIVER_TYPE_UNKNOWN, flags) {
                Ok(dev) => return Ok(dev),
                Err(hr) => last_hr = hr,
            }
        }
    }

    // 2. Fall back to default hardware adapter, then WARP.
    for driver_type in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        for flags in flag_sets {
            match try_create(std::ptr::null_mut(), driver_type, flags) {
                Ok(dev) => return Ok(dev),
                Err(hr) => last_hr = hr,
            }
        }
    }

    Err(format!("D3D11CreateDevice failed: {:#x}", last_hr as u32))
}

/// Xbox GDK `MFResetDXGIDeviceManagerX(IMFDXGIDeviceManager*, IUnknown*, UINT32) -> HRESULT`.
pub unsafe extern "system" fn MFResetDXGIDeviceManagerX(
    pDeviceManager: *mut c_void,
    pDevice: *mut c_void,
    resetToken: u32,
) -> HRESULT {
    if pDeviceManager.is_null() || pDevice.is_null() {
        return E_POINTER;
    }

    // Borrow caller-owned COM pointers without taking ownership (no Release on drop).
    let manager = ManuallyDrop::new(IMFDXGIDeviceManager::from_raw(pDeviceManager));
    let device_unk = ManuallyDrop::new(IUnknown::from_raw(pDevice));

    // First try passing the device directly (succeeds if it is already an ID3D11Device
    // or if Windows accepts it directly).
    match manager.ResetDevice(&*device_unk, resetToken) {
        Ok(()) => {
            crate::xlog_debug!("mfplat: MFResetDXGIDeviceManagerX direct ResetDevice succeeded");
            return 0;
        }
        Err(direct_err) => {
            let direct_hr = direct_err.code().0;

            // Xbox titles pass an ID3D12Device; Windows' IMFDXGIDeviceManager::ResetDevice
            // requires an ID3D11Device. Bridge via a companion D3D11 device on the same
            // adapter LUID so Media Foundation hardware transforms / MFSourceReader work.
            if let Ok(d3d12_dev) = device_unk.cast::<ID3D12Device>() {
                let luid = d3d12_dev.GetAdapterLuid();
                match create_d3d11_bridge(Some(luid)) {
                    Ok(d3d11_unk) => match manager.ResetDevice(&d3d11_unk, resetToken) {
                        Ok(()) => {
                            crate::xlog_info!(
                                "mfplat: MFResetDXGIDeviceManagerX bridged ID3D12Device (LUID {:x}:{:x}) via companion D3D11 device",
                                luid.HighPart,
                                luid.LowPart
                            );
                            return 0;
                        }
                        Err(bridge_err) => {
                            let bridge_hr = bridge_err.code().0;
                            crate::xlog_warn!(
                                "mfplat: MFResetDXGIDeviceManagerX bridged ResetDevice failed: {:#x} (direct was {:#x})",
                                bridge_hr as u32,
                                direct_hr as u32
                            );
                            return bridge_hr;
                        }
                    },
                    Err(e) => {
                        crate::xlog_warn!(
                            "mfplat: MFResetDXGIDeviceManagerX failed to create D3D11 bridge ({e}); returning direct error {:#x}",
                            direct_hr as u32
                        );
                        return direct_hr;
                    }
                }
            }

            crate::xlog_warn!(
                "mfplat: MFResetDXGIDeviceManagerX ResetDevice failed: {:#x}",
                direct_hr as u32
            );
            direct_hr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pe::ImportName;
    use crate::resolver::Source;
    use crate::util::{E_INVALIDARG, S_OK};

    #[test]
    fn routing_and_system_fallback() {
        let name = |s: &str| ImportName::Name { hint: 0, name: s.into() };
        assert_eq!(
            crate::resolver::classify("mfplat.dll", &name("MFResetDXGIDeviceManagerX")),
            Source::Hle(crate::hle::MFPLAT.to_string())
        );
        assert_eq!(
            crate::resolver::classify("mfplat.dll", &name("MFStartup")),
            Source::System("mfplat.dll".to_string())
        );
        assert_eq!(
            crate::resolver::classify("mfplat.dll", &name("MFCreateDXGIDeviceManager")),
            Source::System("mfplat.dll".to_string())
        );
        assert_eq!(
            crate::resolver::classify("mfplat.dll", &name("MFCreateMediaType")),
            Source::System("mfplat.dll".to_string())
        );

        // LoadLibrary + GetProcAddress roundtrip for both HLE and system exports.
        let h = crate::resolver::load_library("mfplat.dll").expect("load_library(mfplat.dll)");
        let hle_fn = crate::resolver::get_proc_address(h, &name("MFResetDXGIDeviceManagerX"))
            .expect("GetProcAddress(MFResetDXGIDeviceManagerX)");
        assert_eq!(hle_fn, MFResetDXGIDeviceManagerX as usize);
        let sys_fn = crate::resolver::get_proc_address(h, &name("MFStartup"))
            .expect("GetProcAddress(MFStartup)");
        assert_ne!(sys_fn, 0);
    }

    #[test]
    fn null_pointers_return_e_pointer() {
        unsafe {
            assert_eq!(
                MFResetDXGIDeviceManagerX(std::ptr::null_mut(), std::ptr::null_mut(), 0),
                E_POINTER
            );
            assert_eq!(
                MFResetDXGIDeviceManagerX(0x1000 as *mut c_void, std::ptr::null_mut(), 1),
                E_POINTER
            );
            assert_eq!(
                MFResetDXGIDeviceManagerX(std::ptr::null_mut(), 0x1000 as *mut c_void, 1),
                E_POINTER
            );
        }
    }

    #[test]
    fn reset_dxgi_device_manager_with_d3d12_device_and_token_validation() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Media::MediaFoundation::{
            MFCreateDXGIDeviceManager, MFShutdown, MFStartup, MFSTARTUP_LITE, MF_VERSION,
        };

        unsafe {
            if MFStartup(MF_VERSION, MFSTARTUP_LITE).is_err() {
                return;
            }

            let mut reset_token: u32 = 0;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            let hr = MFCreateDXGIDeviceManager(&mut reset_token, &mut manager);
            assert!(hr.is_ok(), "MFCreateDXGIDeviceManager failed: {hr:?}");
            let manager = manager.expect("manager");

            let Ok(d3d12_dev) = crate::gpu::agility::create_device(None, D3D_FEATURE_LEVEL_11_0) else {
                let _ = MFShutdown();
                return;
            };

            // Invalid reset token MUST be rejected.
            let bad_token = reset_token.wrapping_add(1);
            let hr_bad = MFResetDXGIDeviceManagerX(
                manager.as_raw(),
                d3d12_dev.as_raw(),
                bad_token,
            );
            assert_eq!(
                hr_bad, E_INVALIDARG,
                "invalid token should return E_INVALIDARG, got {:#x}",
                hr_bad as u32
            );

            // Valid reset token with ID3D12Device must succeed.
            let hr_ok = MFResetDXGIDeviceManagerX(
                manager.as_raw(),
                d3d12_dev.as_raw(),
                reset_token,
            );
            assert_eq!(hr_ok, S_OK, "MFResetDXGIDeviceManagerX failed: {:#x}", hr_ok as u32);

            // Verify the device manager now has a usable device handle.
            let handle = manager
                .OpenDeviceHandle()
                .expect("OpenDeviceHandle should succeed after MFResetDXGIDeviceManagerX");
            manager
                .TestDevice(handle)
                .expect("TestDevice should succeed on reset device");
            manager
                .CloseDeviceHandle(handle)
                .expect("CloseDeviceHandle should succeed");

            // Also verify the D3D11 bridge path works end-to-end on the same adapter LUID.
            let luid = d3d12_dev.GetAdapterLuid();
            let d3d11_unk = create_d3d11_bridge(Some(luid))
                .expect("create_d3d11_bridge should succeed");
            manager
                .ResetDevice(&d3d11_unk, reset_token)
                .expect("ResetDevice with bridged D3D11 device should succeed");
            let handle2 = manager
                .OpenDeviceHandle()
                .expect("OpenDeviceHandle should succeed on bridged D3D11 device");
            manager
                .TestDevice(handle2)
                .expect("TestDevice should succeed on bridged D3D11 device");
            manager
                .CloseDeviceHandle(handle2)
                .expect("CloseDeviceHandle should succeed");

            drop(manager);
            drop(d3d11_unk);
            drop(d3d12_dev);
            let _ = MFShutdown();
        }
    }
}


// Victory-derived Media Foundation source resolver proxy. See the VFS translation
// in `local()` before a local file URL reaches the host resolver.
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};





const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
/// What Media Foundation answers for a URL scheme it has no handler for.
const UNSUPPORTED_SCHEME: HRESULT = 0xC00D_36C3_u32 as i32;

const IID_IUNKNOWN: [u8; 16] = guid(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_RESOLVER: [u8; 16] = guid(
    0xFBE5_A32D,
    0xA497,
    0x4B61,
    [0xBB, 0x85, 0x97, 0xB1, 0xA8, 0x48, 0xA6, 0xE3],
);

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

// The URL rule.

/// Where a resolver may read `url` from: `Ok` with the NUL-terminated
/// string to hand to the host, or `Err((scheme, host))` for anything that
/// is not a local file.
fn local(url: &[u16]) -> Result<Vec<u16>, (String, String)> {
    let text = String::from_utf16_lossy(url);
    let drive = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let host_of = |rest: &str| rest.split(['/', '\\']).next().unwrap_or("").to_string();
    let path = if let Some(rest) = text.strip_prefix(r"\\?\") {
        if !drive(rest) {
            return Err(("unc".into(), host_of(rest.trim_start_matches("UNC\\"))));
        }
        text.clone()
    } else if let Some(rest) = text.strip_prefix(r"\\").or_else(|| text.strip_prefix("//")) {
        return Err(("unc".into(), host_of(rest)));
    } else if drive(&text) {
        text.clone()
    } else if let Some((scheme, rest)) = text.split_once(':').filter(|(s, _)| {
        s.len() > 1
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    }) {
        let scheme = scheme.to_ascii_lowercase();
        let authority = rest.strip_prefix("//");
        if scheme != "file" {
            return Err((scheme, authority.map(host_of).unwrap_or_default()));
        }
        let path = match authority {
            Some(rest) => {
                let host = host_of(rest);
                if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                    return Err((scheme, host));
                }
                &rest[host.len()..]
            }
            None => rest,
        };
        let path = percent_decode(path.trim_start_matches(['/', '\\'])).replace('/', "\\");
        if !drive(&path) {
            // A local file without a drive: the host resolves it as is.
            return Ok(with_nul(url));
        }
        path
    } else {
        // A relative path.
        return Ok(with_nul(url));
    };
    Ok({
        let host_path = crate::vfs::get()
            .and_then(|v| v.real_path_for_read(&path))
            .unwrap_or(path);
        with_nul(&host_path.encode_utf16().collect::<Vec<_>>())
    })
}

fn with_nul(s: &[u16]) -> Vec<u16> {
    s.iter().copied().chain([0]).collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The URL to give the host, or the refusal (logged once per scheme and
/// host).
fn checked(url: *const u16) -> Result<Vec<u16>, HRESULT> {
    if url.is_null() {
        return Err(E_POINTER);
    }
    // SAFETY: a NUL-terminated wide string from the title.
    let len = (0..)
        .take_while(|&i| unsafe { url.add(i).read() } != 0)
        .count();
    // SAFETY: as above, `len` characters before the NUL.
    let wide = unsafe { std::slice::from_raw_parts(url, len) };
    local(wide).map_err(|(scheme, host)| {
        static REFUSED: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);
        let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
        if refused
            .get_or_insert_with(HashSet::new)
            .insert((scheme.clone(), host.clone()))
        {
            crate::xlog_warn!("media: refused non-local URL scheme={scheme} host={host}");
        }
        UNSUPPORTED_SCHEME
    })
}

// The proxy resolver.

/// The host's resolver, called through its method table.
#[repr(C)]
struct Host {
    vtable: *const usize,
}

impl Host {
    /// Method `n` of the host object, typed by the caller.
    unsafe fn method<F: Copy>(this: *mut Host, n: usize) -> F {
        // SAFETY: the caller names the slot's real signature; `this` is the
        // host's live resolver.
        unsafe { std::mem::transmute_copy(&*(*this).vtable.add(n)) }
    }
}

#[repr(C)]
pub struct Resolver {
    vtable: &'static Methods,
    host: *mut Host,
    refs: AtomicU32,
}

type Unknown = *mut c_void;

#[repr(C)]
struct Methods {
    query_interface:
        extern "system" fn(*mut Resolver, *const [u8; 16], *mut *mut c_void) -> HRESULT,
    add_ref: extern "system" fn(*mut Resolver) -> u32,
    release: extern "system" fn(*mut Resolver) -> u32,
    create_from_url: extern "system" fn(
        *mut Resolver,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT,
    create_from_stream: extern "system" fn(
        *mut Resolver,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT,
    begin_from_url: extern "system" fn(
        *mut Resolver,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT,
    end_from_url: extern "system" fn(*mut Resolver, Unknown, *mut u32, *mut Unknown) -> HRESULT,
    begin_from_stream: extern "system" fn(
        *mut Resolver,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT,
    end_from_stream: extern "system" fn(*mut Resolver, Unknown, *mut u32, *mut Unknown) -> HRESULT,
    cancel: extern "system" fn(*mut Resolver, Unknown) -> HRESULT,
}

static METHODS: Methods = Methods {
    query_interface,
    add_ref,
    release,
    create_from_url,
    create_from_stream,
    begin_from_url,
    end_from_url,
    begin_from_stream,
    end_from_stream,
    cancel,
};

fn host(this: *mut Resolver) -> *mut Host {
    // SAFETY: title code calls methods on a proxy we made, alive until its
    // last release.
    unsafe { (*this).host }
}

/// Only the resolver's own interfaces: anything else could hand out the
/// host object unguarded.
extern "system" fn query_interface(
    this: *mut Resolver,
    iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let iid = unsafe { *iid };
    let found = iid == IID_IUNKNOWN || iid == IID_RESOLVER;
    // SAFETY: checked non-null.
    unsafe {
        out.write(if found {
            this.cast()
        } else {
            std::ptr::null_mut()
        })
    };
    if !found {
        return E_NOINTERFACE;
    }
    add_ref(this);
    S_OK
}

extern "system" fn add_ref(this: *mut Resolver) -> u32 {
    // SAFETY: as in `host`.
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

extern "system" fn release(this: *mut Resolver) -> u32 {
    // SAFETY: as in `host`.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `MFCreateSourceResolver`; this was the last
        // reference, to the proxy and to the host object it holds.
        let proxy = unsafe { Box::from_raw(this) };
        // SAFETY: slot 2 is `Release`.
        let release: extern "system" fn(*mut Host) -> u32 = unsafe { Host::method(proxy.host, 2) };
        release(proxy.host);
    }
    left
}

extern "system" fn create_from_url(
    this: *mut Resolver,
    url: *const u16,
    flags: u32,
    props: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let url = match checked(url) {
        Ok(url) => url,
        Err(error) => {
            if !object.is_null() {
                // SAFETY: the title's out parameter.
                unsafe { object.write(std::ptr::null_mut()) };
            }
            return error;
        }
    };
    let host = host(this);
    // SAFETY: slot 3 is `CreateObjectFromURL`.
    let f: extern "system" fn(
        *mut Host,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 3) };
    f(host, url.as_ptr(), flags, props, kind, object)
}

extern "system" fn begin_from_url(
    this: *mut Resolver,
    url: *const u16,
    flags: u32,
    props: Unknown,
    cookie: *mut Unknown,
    callback: Unknown,
    state: Unknown,
) -> HRESULT {
    let url = match checked(url) {
        Ok(url) => url,
        Err(error) => return error,
    };
    let host = host(this);
    // SAFETY: slot 5 is `BeginCreateObjectFromURL`.
    let f: extern "system" fn(
        *mut Host,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 5) };
    f(host, url.as_ptr(), flags, props, cookie, callback, state)
}

extern "system" fn create_from_stream(
    this: *mut Resolver,
    stream: Unknown,
    url: *const u16,
    flags: u32,
    props: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 4 is `CreateObjectFromByteStream`; the URL is only a
    // hint for picking a handler.
    let f: extern "system" fn(
        *mut Host,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 4) };
    f(host, stream, url, flags, props, kind, object)
}

extern "system" fn end_from_url(
    this: *mut Resolver,
    result: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 6 is `EndCreateObjectFromURL`.
    let f: extern "system" fn(*mut Host, Unknown, *mut u32, *mut Unknown) -> HRESULT =
        unsafe { Host::method(host, 6) };
    f(host, result, kind, object)
}

#[allow(clippy::too_many_arguments)]
extern "system" fn begin_from_stream(
    this: *mut Resolver,
    stream: Unknown,
    url: *const u16,
    flags: u32,
    props: Unknown,
    cookie: *mut Unknown,
    callback: Unknown,
    state: Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 7 is `BeginCreateObjectFromByteStream`.
    let f: extern "system" fn(
        *mut Host,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 7) };
    f(host, stream, url, flags, props, cookie, callback, state)
}

extern "system" fn end_from_stream(
    this: *mut Resolver,
    result: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 8 is `EndCreateObjectFromByteStream`.
    let f: extern "system" fn(*mut Host, Unknown, *mut u32, *mut Unknown) -> HRESULT =
        unsafe { Host::method(host, 8) };
    f(host, result, kind, object)
}

extern "system" fn cancel(this: *mut Resolver, cookie: Unknown) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 9 is `CancelObjectCreation`.
    let f: extern "system" fn(*mut Host, Unknown) -> HRESULT = unsafe { Host::method(host, 9) };
    f(host, cookie)
}

/// The host's resolver behind our proxy.
pub extern "system" fn MFCreateSourceResolver(out: *mut *mut Resolver) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    static CREATE: OnceLock<Option<usize>> = OnceLock::new();
    let Some(create) = *CREATE.get_or_init(|| {
        let module = crate::resolver::system_module("mfplat.dll")?;
        crate::resolver::system_proc_in(
            module,
            &crate::pe::ImportName::Name {
                hint: 0,
                name: "MFCreateSourceResolver".into(),
            },
        )
    }) else {
        return UNSUPPORTED_SCHEME;
    };
    // SAFETY: the host's `MFCreateSourceResolver(IMFSourceResolver**)`.
    let create: extern "system" fn(*mut *mut Host) -> HRESULT =
        unsafe { std::mem::transmute(create) };
    let mut host = std::ptr::null_mut();
    let hr = create(&mut host);
    if hr < 0 || host.is_null() {
        return hr;
    }
    let proxy = Box::new(Resolver {
        vtable: &METHODS,
        host,
        refs: AtomicU32::new(1),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(proxy)) };
    S_OK
}



