//! Local named pipes with overlapped deadlines. This module creates no threads.
use std::{
    ffi::c_void,
    io::{self, Read, Write},
    path::Path,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
type Handle = *mut c_void;
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const SecurityAttributes,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CreateNamedPipeW(
        name: *const u16,
        open: u32,
        mode: u32,
        instances: u32,
        out_size: u32,
        in_size: u32,
        timeout: u32,
        security: *const SecurityAttributes,
    ) -> Handle;
    fn ConnectNamedPipe(pipe: Handle, overlap: *mut Overlapped) -> i32;
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
    fn GetNamedPipeClientProcessId(pipe: Handle, pid: *mut u32) -> i32;
    fn ReadFile(
        file: Handle,
        buffer: *mut u8,
        len: u32,
        read: *mut u32,
        overlap: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        file: Handle,
        buffer: *const u8,
        len: u32,
        written: *mut u32,
        overlap: *mut Overlapped,
    ) -> i32;
    fn GetOverlappedResult(
        file: Handle,
        overlap: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn CancelIoEx(file: Handle, overlap: *const Overlapped) -> i32;
    fn CreateEventW(
        security: *const SecurityAttributes,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        name: *mut u16,
        len: *mut u32,
    ) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: *mut c_void,
        len: u32,
        required: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, string: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}
struct Owned(Handle);
unsafe impl Send for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn owned(handle: Handle) -> io::Result<Owned> {
    if handle.is_null() || handle as isize == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Owned(handle))
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn session(pid: u32) -> io::Result<u32> {
    let mut id = 0;
    if unsafe { ProcessIdToSessionId(pid, &mut id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(id)
}
fn sid(process: Handle) -> io::Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, 8, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    let mut len = 0;
    unsafe {
        GetTokenInformation(token.0, 1, null_mut(), 0, &mut len);
    }
    if len == 0 || len > 65536 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut buffer = vec![0u8; len as usize];
    if unsafe { GetTokenInformation(token.0, 1, buffer.as_mut_ptr().cast(), len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ptr = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<*mut c_void>()) };
    let mut string = null_mut();
    if unsafe { ConvertSidToStringSidW(ptr, &mut string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut size = 0;
    while size < 256 && unsafe { *string.add(size) } != 0 {
        size += 1;
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(string, size) })
        .map_err(|_| io::ErrorKind::InvalidData.into());
    unsafe {
        LocalFree(string.cast());
    }
    result
}
pub fn endpoint() -> io::Result<String> {
    let identity = sid(unsafe { GetCurrentProcess() })?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in identity.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!(
        "\\\\.\\pipe\\kanade-ime.{hash:016x}.{}.engine",
        session(unsafe { GetCurrentProcessId() })?
    ))
}
fn verify(pipe: Handle, server: bool, expected: Option<&Path>) -> io::Result<()> {
    let mut pid = 0;
    let ok = unsafe {
        if server {
            GetNamedPipeServerProcessId(pipe, &mut pid)
        } else {
            GetNamedPipeClientProcessId(pipe, &mut pid)
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = owned(unsafe { OpenProcess(0x1000, 0, pid) })?;
    if sid(process.0)? != sid(unsafe { GetCurrentProcess() })?
        || session(pid)? != session(unsafe { GetCurrentProcessId() })?
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    if let Some(expected) = expected {
        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let actual =
            String::from_utf16(&buffer[..len as usize]).map_err(|_| io::ErrorKind::InvalidData)?;
        if !actual
            .trim_start_matches("\\\\?\\")
            .eq_ignore_ascii_case(expected.to_string_lossy().trim_start_matches("\\\\?\\"))
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}
pub struct Pipe {
    handle: Owned,
    deadline: Option<Instant>,
}
impl Pipe {
    /// Management is restricted to the installed settings executable. Input hosts
    /// retain conversion access without gaining access to stored learning records.
    pub fn verify_management_peer(&self, expected_client: &Path) -> io::Result<()> {
        verify(self.handle.0, false, Some(expected_client))
    }

    pub fn connect(expected_server: &Path) -> io::Result<Self> {
        let name = wide(&endpoint()?);
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 50);
        }
        let handle = owned(unsafe {
            CreateFileW(
                name.as_ptr(),
                0xc0000000,
                0,
                null(),
                3,
                0x40000000,
                null_mut(),
            )
        })?;
        verify(handle.0, true, Some(expected_server))?;
        Ok(Self {
            handle,
            deadline: Some(Instant::now() + crate::KEY_TIMEOUT),
        })
    }
    pub fn begin(&mut self, timeout: Duration) {
        self.deadline = Some(Instant::now() + timeout);
    }
    /// Server side of a long-lived connection: waits without limit for a
    /// frame to begin, then requires the whole frame within `limit`.
    pub fn read_idle_frame(&mut self, limit: Duration) -> io::Result<Option<Vec<u8>>> {
        self.deadline = None;
        crate::read_idle_frame(self, |pipe| {
            pipe.begin(limit);
            Ok(())
        })
    }
    fn complete(&self, overlap: &mut Overlapped, pending: bool) -> io::Result<usize> {
        let timeout = self
            .deadline
            .map(|d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(0xfffffffe) as u32
            })
            .unwrap_or(u32::MAX);
        if pending && unsafe { WaitForSingleObject(overlap.event, timeout) } != 0 {
            unsafe {
                CancelIoEx(self.handle.0, overlap);
                let mut ignored = 0;
                GetOverlappedResult(self.handle.0, overlap, &mut ignored, 1);
            }
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut transferred = 0;
        if unsafe { GetOverlappedResult(self.handle.0, overlap, &mut transferred, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }
    fn operation(&self, read: Option<&mut [u8]>, write: Option<&[u8]>) -> io::Result<usize> {
        let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let mut overlap = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: event.0,
        };
        let ok = unsafe {
            if let Some(buffer) = read {
                ReadFile(
                    self.handle.0,
                    buffer.as_mut_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    &mut overlap,
                )
            } else if let Some(buffer) = write {
                WriteFile(
                    self.handle.0,
                    buffer.as_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    &mut overlap,
                )
            } else {
                return Ok(0);
            }
        };
        if ok == 0 && io::Error::last_os_error().raw_os_error() != Some(997) {
            return Err(io::Error::last_os_error());
        }
        self.complete(&mut overlap, ok == 0)
    }
}
impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.operation(Some(buf), None)
    }
}
impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.operation(None, Some(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl crate::frontend::Transport for Pipe {
    fn exchange(&mut self, request: &crate::Request) -> io::Result<crate::Response> {
        self.begin(crate::KEY_TIMEOUT);
        crate::send(self, request)?;
        crate::receive(self)
    }
    fn notify(&mut self, request: &crate::Request) -> io::Result<()> {
        self.begin(crate::KEY_TIMEOUT);
        crate::send(self, request)
    }
}
pub struct Listener {
    next: Owned,
    name: Vec<u16>,
}
impl Listener {
    pub fn bind() -> io::Result<Self> {
        let name = wide(&endpoint()?);
        Ok(Self {
            next: instance(&name, true)?,
            name,
        })
    }
    pub fn accept(&mut self) -> io::Result<Pipe> {
        let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let mut overlap = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: event.0,
        };
        let ok = unsafe { ConnectNamedPipe(self.next.0, &mut overlap) };
        let error = io::Error::last_os_error().raw_os_error();
        if ok == 0 && error == Some(997) {
            if unsafe { WaitForSingleObject(event.0, u32::MAX) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut count = 0;
            if unsafe { GetOverlappedResult(self.next.0, &mut overlap, &mut count, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
        } else if ok == 0 && error != Some(535) {
            return Err(io::Error::last_os_error());
        }
        let handle = std::mem::replace(&mut self.next, instance(&self.name, false)?);
        verify(handle.0, false, None)?;
        Ok(Pipe {
            handle,
            deadline: None,
        })
    }
}
fn instance(name: &[u16], first: bool) -> io::Result<Owned> {
    let user = sid(unsafe { GetCurrentProcess() })?;
    let sddl = wide(&format!(
        "D:(A;;GA;;;{user})(A;;0x0012019b;;;S-1-15-2-1)(A;;0x0012019b;;;S-1-15-2-2)S:(ML;;NW;;;LW)"
    ));
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            3 | 0x40000000 | if first { 0x00080000 } else { 0 },
            8,
            255,
            65536,
            65536,
            50,
            &attributes,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    owned(handle)
}
