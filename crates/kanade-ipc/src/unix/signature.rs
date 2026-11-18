//! Peer identity is obtained from the kernel audit token, then checked by Security.framework.
use super::*;
use std::ffi::{OsStr, c_void};
type Ref = *const c_void;
struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Ref);
    fn CFDataCreate(allocator: Ref, bytes: *const u8, len: isize) -> Ref;
    fn CFDataGetLength(data: Ref) -> isize;
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        keys_callback: Ref,
        values_callback: Ref,
    ) -> Ref;
    fn CFDictionaryGetValue(dict: Ref, key: Ref) -> Ref;
    fn CFArrayGetCount(array: Ref) -> isize;
    fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
    fn CFURLCopyFileSystemPath(url: Ref, style: isize) -> Ref;
    fn CFStringGetCString(string: Ref, buffer: *mut u8, len: isize, encoding: u32) -> bool;
}
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecGuestAttributeAudit: Ref;
    static kSecCodeInfoCertificates: Ref;
    static kSecCodeInfoMainExecutable: Ref;
    fn SecCodeCopySelf(flags: u32, code: *mut Ref) -> i32;
    fn SecCodeCopyGuestWithAttributes(
        host: Ref,
        attributes: Ref,
        flags: u32,
        code: *mut Ref,
    ) -> i32;
    fn SecCodeCheckValidity(code: Ref, flags: u32, requirement: Ref) -> i32;
    fn SecCodeCopyStaticCode(code: Ref, flags: u32, static_code: *mut Ref) -> i32;
    fn SecCodeCopySigningInformation(code: Ref, flags: u32, info: *mut Ref) -> i32;
    fn SecCertificateCopyData(certificate: Ref) -> Ref;
}
fn denied<T>() -> io::Result<T> {
    Err(io::ErrorKind::PermissionDenied.into())
}
fn information(code: Ref) -> io::Result<Owned> {
    let mut static_code = std::ptr::null();
    if unsafe { SecCodeCopyStaticCode(code, 0, &mut static_code) } != 0 {
        return denied();
    }
    let static_code = Owned(static_code);
    let mut info = std::ptr::null();
    if unsafe { SecCodeCopySigningInformation(static_code.0, 2, &mut info) } != 0 {
        return denied();
    }
    Ok(Owned(info))
}
fn certificate(info: Ref) -> io::Result<Vec<u8>> {
    let certificates = unsafe { CFDictionaryGetValue(info, kSecCodeInfoCertificates) };
    if certificates.is_null() || unsafe { CFArrayGetCount(certificates) } < 1 {
        return denied();
    }
    let leaf = unsafe { CFArrayGetValueAtIndex(certificates, 0) };
    let data = Owned(unsafe { SecCertificateCopyData(leaf) });
    if data.0.is_null() {
        return denied();
    }
    let len = unsafe { CFDataGetLength(data.0) };
    if !(1..=65536).contains(&len) {
        return denied();
    }
    Ok(unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), len as usize) }.to_vec())
}
pub(super) fn verify(stream: &UnixStream) -> io::Result<()> {
    let mut token = [0u32; 8];
    let mut len = std::mem::size_of_val(&token) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0,
            6,
            token.as_mut_ptr().cast(),
            &mut len,
        )
    } != 0
        || len as usize != std::mem::size_of_val(&token)
    {
        return denied();
    }
    // Unsigned development binaries need explicit consent in their launch environment.
    if cfg!(debug_assertions) && std::env::var_os("KANADE_ALLOW_UNSIGNED").is_some_and(|v| v == "1")
    {
        return Ok(());
    }
    let audit =
        Owned(unsafe { CFDataCreate(std::ptr::null(), token.as_ptr().cast(), len as isize) });
    let keys = [unsafe { kSecGuestAttributeAudit }];
    let values = [audit.0];
    let attrs = Owned(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        )
    });
    if audit.0.is_null() || attrs.0.is_null() {
        return denied();
    }
    let mut peer = std::ptr::null();
    if unsafe { SecCodeCopyGuestWithAttributes(std::ptr::null(), attrs.0, 0, &mut peer) } != 0 {
        return denied();
    }
    let peer = Owned(peer);
    if unsafe { SecCodeCheckValidity(peer.0, 0, std::ptr::null()) } != 0 {
        return denied();
    }
    let peer_info = information(peer.0)?;
    let mut own = std::ptr::null();
    if unsafe { SecCodeCopySelf(0, &mut own) } != 0 {
        return denied();
    }
    let own = Owned(own);
    let own_info = information(own.0)?;
    if certificate(peer_info.0)? != certificate(own_info.0)? {
        return denied();
    }
    let url = unsafe { CFDictionaryGetValue(peer_info.0, kSecCodeInfoMainExecutable) };
    if url.is_null() {
        return denied();
    }
    let path = Owned(unsafe { CFURLCopyFileSystemPath(url, 0) });
    let mut buffer = [0u8; 4096];
    if path.0.is_null()
        || !unsafe {
            CFStringGetCString(
                path.0,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                0x08000100,
            )
        }
    {
        return denied();
    }
    let end = buffer
        .iter()
        .position(|b| *b == 0)
        .ok_or(io::ErrorKind::PermissionDenied)?;
    let peer_path = Path::new(OsStr::from_bytes(&buffer[..end])).canonicalize()?;
    let own_path = std::env::current_exe()?.canonicalize()?;
    if package_root(&peer_path) != package_root(&own_path)
        || package_root(&own_path).is_none()
        || !peer_path.file_name().is_some_and(|n| {
            ["kanade-engine", "kanade-imk", "kanade-ui", "kanade-eval"]
                .iter()
                .any(|v| n == *v)
        })
    {
        return denied();
    }
    Ok(())
}

fn package_root(path: &Path) -> Option<&Path> {
    path.ancestors()
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .last()
}
