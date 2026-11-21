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
    fn PeekNamedPipe(
        pipe: Handle,
        buffer: *mut u8,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
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
    fn ConvertStringSidToSidW(string: *const u16, sid: *mut *mut c_void) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        object: Handle,
        kind: i32,
        information: u32,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        descriptor: *mut *mut c_void,
    ) -> u32;
    fn SetSecurityInfo(
        object: Handle,
        kind: i32,
        information: u32,
        owner: *const c_void,
        group: *const c_void,
        dacl: *const c_void,
        sacl: *const c_void,
    ) -> u32;
    fn SetEntriesInAclW(
        count: u32,
        entries: *const ExplicitAccess,
        old: *const c_void,
        new: *mut *mut c_void,
    ) -> u32;
}
#[repr(C)]
struct Trustee {
    multiple: *mut c_void,
    operation: i32,
    form: i32,
    kind: i32,
    name: *mut c_void,
}
#[repr(C)]
struct ExplicitAccess {
    permissions: u32,
    mode: i32,
    inheritance: u32,
    trustee: Trustee,
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
/// The installed engine's endpoint for this user and logon session.
pub fn endpoint() -> io::Result<String> {
    named("kanade-ime")
}
/// The endpoint of an isolated engine. A test or development instance passes
/// its own label, so it never takes or reaches the installed engine's pipe.
/// The user and session stay part of the name.
pub fn instance_endpoint(label: &str) -> io::Result<String> {
    if label.is_empty()
        || label.len() > 64
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    named(&format!("kanade-ime.{label}"))
}
fn named(prefix: &str) -> io::Result<String> {
    let identity = sid(unsafe { GetCurrentProcess() })?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in identity.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!(
        "\\\\.\\pipe\\{prefix}.{hash:016x}.{}.engine",
        session(unsafe { GetCurrentProcessId() })?
    ))
}
/// The two groups every packaged application belongs to: the ordinary ones
/// and those with fewer privileges. The pipe admits the same two.
const PACKAGES: [&str; 2] = ["S-1-15-2-1", "S-1-15-2-2"];

/// Adds `access` for every packaged application to the access list of a
/// kernel object. `object` must allow reading and writing that list.
fn grant_to_packages(object: Handle, access: u32) -> io::Result<()> {
    // SE_KERNEL_OBJECT and DACL_SECURITY_INFORMATION.
    const KERNEL_OBJECT: i32 = 6;
    const ACCESS_LIST: u32 = 4;
    // These three report their error as their result.
    let done = |code: u32| match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code as i32)),
    };
    let mut current = null_mut();
    let mut descriptor = null_mut();
    done(unsafe {
        GetSecurityInfo(
            object,
            KERNEL_OBJECT,
            ACCESS_LIST,
            null_mut(),
            null_mut(),
            &mut current,
            null_mut(),
            &mut descriptor,
        )
    })?;
    let mut groups = [null_mut(); PACKAGES.len()];
    let result = (|| {
        // An object without a list is open to everyone already. A list made
        // of these entries alone would close it to everyone else.
        if current.is_null() {
            return Ok(());
        }
        for (group, name) in groups.iter_mut().zip(PACKAGES) {
            if unsafe { ConvertStringSidToSidW(wide(name).as_ptr(), group) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let entries = groups.map(|group| ExplicitAccess {
            permissions: access,
            // GRANT_ACCESS: merged with what the group already has.
            mode: 1,
            inheritance: 0,
            trustee: Trustee {
                multiple: null_mut(),
                operation: 0,
                // TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP.
                form: 0,
                kind: 5,
                name: group,
            },
        });
        let mut extended = null_mut();
        done(unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                current,
                &mut extended,
            )
        })?;
        let set = done(unsafe {
            SetSecurityInfo(
                object,
                KERNEL_OBJECT,
                ACCESS_LIST,
                null(),
                null(),
                extended,
                null(),
            )
        });
        unsafe {
            LocalFree(extended);
        }
        set
    })();
    unsafe {
        for group in groups {
            LocalFree(group);
        }
        LocalFree(descriptor);
    }
    result
}

/// Lets the sandboxed peers that the pipe admits identify this process.
///
/// A frontend checks which program serves the pipe, and as which user,
/// before it sends a key. For that it opens the serving process and its
/// token, which a packaged application may not do by default: such a host
/// could open the pipe and still never connect. This grants every package
/// the two rights that check needs and nothing more: the limited query
/// right on the process, and the right to read its token. The first shows
/// the image name, the command line, the exit status and the process's time
/// and I/O counters; the second the user, groups and privileges. Neither
/// allows reading memory, waiting for, or changing the process. The
/// counters move with every request, so a package that polls them can tell
/// when the user types, though not what; a password field never reaches
/// this process. Call it once in a process that serves an endpoint.
pub fn identify_to_packages() -> io::Result<()> {
    // PROCESS_QUERY_LIMITED_INFORMATION. The pseudo-handle has every right.
    grant_to_packages(unsafe { GetCurrentProcess() }, 0x1000)?;
    let mut token = null_mut();
    // READ_CONTROL and WRITE_DAC, to read and write the token's access list.
    if unsafe { OpenProcessToken(GetCurrentProcess(), 0x0006_0000, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    // TOKEN_QUERY.
    grant_to_packages(token.0, 0x0008)
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
        Self::connect_to(&endpoint()?, expected_server)
    }
    /// Connects to a named endpoint, for an isolated instance.
    pub fn connect_to(endpoint: &str, expected_server: &Path) -> io::Result<Self> {
        let name = wide(endpoint);
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
    /// Whether the peer still holds its end. Reads nothing and never blocks.
    /// Unread bytes outside an exchange are not a reply to anything, so they
    /// count as a broken connection too.
    pub fn peer_open(&self) -> bool {
        let mut available = 0;
        unsafe {
            PeekNamedPipe(
                self.handle.0,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            ) != 0
                && available == 0
        }
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
    fn is_open(&mut self) -> bool {
        self.peer_open()
    }
}
/// Why `Listener::accept` returned no connection.
#[derive(Debug)]
pub enum Refusal {
    /// This peer only: it left before it was accepted, or it is not this
    /// user's process in this session. The listener is unaffected.
    Peer(io::Error),
    /// No pipe instance could be offered. Nobody can connect until a later
    /// call succeeds.
    Listener(io::Error),
}
pub struct Listener {
    /// The instance offered to the next peer. Absent after one could not be
    /// created; `accept` creates it again.
    next: Option<Owned>,
    name: Vec<u16>,
}
impl Listener {
    pub fn bind() -> io::Result<Self> {
        Self::bind_to(&endpoint()?)
    }
    /// Listens on a named endpoint, for an isolated instance.
    pub fn bind_to(endpoint: &str) -> io::Result<Self> {
        let name = wide(endpoint);
        Ok(Self {
            next: Some(instance(&name, true)?),
            name,
        })
    }
    /// Waits for the next peer. A refusal never ends the listener: call again.
    pub fn accept(&mut self) -> Result<Pipe, Refusal> {
        let offered = match &self.next {
            Some(offered) => offered.0,
            None => {
                self.next
                    .insert(instance(&self.name, false).map_err(Refusal::Listener)?)
                    .0
            }
        };
        // Without an event nothing can be waited for; that is no peer's fault.
        let event =
            owned(unsafe { CreateEventW(null(), 1, 0, null()) }).map_err(Refusal::Listener)?;
        if let Err(error) = connect(offered, &event) {
            // A peer that opened the pipe and left before this call leaves the
            // instance unusable. Offer a new one in its place.
            self.next = instance(&self.name, false).ok();
            return Err(Refusal::Peer(error));
        }
        // Offer the next instance before checking this peer, so that another
        // one is not turned away meanwhile. If none can be created this peer
        // is still served, and the next call reports the listener's failure.
        let connected = match instance(&self.name, false) {
            Ok(fresh) => self.next.replace(fresh),
            Err(_) => self.next.take(),
        }
        .ok_or_else(|| Refusal::Listener(io::ErrorKind::NotFound.into()))?;
        verify(connected.0, false, None).map_err(Refusal::Peer)?;
        Ok(Pipe {
            handle: connected,
            deadline: None,
        })
    }
}
/// Completes one peer's connection to an offered instance.
fn connect(pipe: Handle, event: &Owned) -> io::Result<()> {
    let mut overlap = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: event.0,
    };
    let ok = unsafe { ConnectNamedPipe(pipe, &mut overlap) };
    let error = io::Error::last_os_error();
    match (ok, error.raw_os_error()) {
        // ERROR_IO_PENDING: no peer yet.
        (0, Some(997)) => {
            if unsafe { WaitForSingleObject(event.0, u32::MAX) } != 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    CancelIoEx(pipe, &overlap);
                    let mut ignored = 0;
                    GetOverlappedResult(pipe, &mut overlap, &mut ignored, 1);
                }
                return Err(error);
            }
            let mut count = 0;
            if unsafe { GetOverlappedResult(pipe, &mut overlap, &mut count, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        // ERROR_PIPE_CONNECTED: the peer arrived before this call.
        (0, Some(535)) => Ok(()),
        (0, _) => Err(error),
        _ => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn private_endpoint() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        instance_endpoint(&format!(
            "unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap()
    }
    /// Opens the pipe as any process of this user can, without a handshake.
    fn open(endpoint: &str) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint)
            .unwrap()
    }
    fn this_process() -> std::path::PathBuf {
        std::env::current_exe().unwrap()
    }

    #[test]
    fn an_isolated_endpoint_is_never_the_installed_one() {
        let installed = endpoint().unwrap();
        let isolated = instance_endpoint("test-1_a").unwrap();
        assert!(installed.starts_with(r"\\.\pipe\kanade-ime."));
        assert!(isolated.starts_with(r"\\.\pipe\kanade-ime.test-1_a."));
        assert_ne!(installed, isolated);
        // Both keep the user and the session, so they end alike.
        let tail = |name: &str| name.rsplitn(4, '.').take(3).collect::<Vec<_>>().join(".");
        assert_eq!(tail(&installed), tail(&isolated));
        // A label cannot leave the name it was given.
        for label in ["", "a.b", r"a\b", "a/b", "a b", "名前", &"x".repeat(65)] {
            assert!(instance_endpoint(label).is_err(), "{label}");
        }
    }
    #[test]
    fn a_peer_that_left_before_it_was_accepted_does_not_end_the_listener() {
        let name = private_endpoint();
        let mut listener = Listener::bind_to(&name).unwrap();
        for _ in 0..50 {
            // Its process ended, or its handshake gave up, right after opening.
            drop(open(&name));
            assert!(matches!(listener.accept(), Err(Refusal::Peer(_))));
        }
        // The next peer is served as if nothing had happened.
        let mut client = Pipe::connect_to(&name, &this_process()).unwrap();
        let mut server = listener.accept().unwrap();
        server.begin(Duration::from_secs(2));
        crate::write_frame(&mut client, b"ping").unwrap();
        assert_eq!(crate::read_frame(&mut server).unwrap(), b"ping");
    }
    #[test]
    fn peers_that_leave_at_any_point_leave_the_listener_serving() {
        let name = private_endpoint();
        let mut listener = Listener::bind_to(&name).unwrap();
        let peers = {
            let name = name.clone();
            std::thread::spawn(move || {
                let mut opened = 0;
                while opened < 200 {
                    // No instance is free while the last peer is being replaced.
                    let Ok(mut pipe) = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&name)
                    else {
                        std::thread::yield_now();
                        continue;
                    };
                    opened += 1;
                    if opened % 2 == 0 {
                        let _ = pipe.write_all(&[3, 0, 0]);
                    }
                }
            })
        };
        let mut accepted = 0;
        let mut refused = 0;
        while accepted + refused < 200 {
            match listener.accept() {
                Ok(_) => accepted += 1,
                Err(Refusal::Peer(_)) => refused += 1,
                Err(Refusal::Listener(error)) => panic!("the listener failed: {error}"),
            }
        }
        peers.join().unwrap();
        let _client = Pipe::connect_to(&name, &this_process()).unwrap();
        assert!(listener.accept().is_ok());
    }
    #[test]
    fn letting_packages_identify_this_process_keeps_every_other_access() {
        // Whether a sandboxed peer can then identify the process is tested
        // with a real one, against the engine. Here: the grants are
        // additions, can be repeated, and leave this user's access as it was.
        identify_to_packages().unwrap();
        identify_to_packages().unwrap();
        let name = private_endpoint();
        let mut listener = Listener::bind_to(&name).unwrap();
        let _client = Pipe::connect_to(&name, &this_process()).unwrap();
        assert!(listener.accept().is_ok());
    }
    #[test]
    fn a_closed_peer_is_noticed_without_sending() {
        let name = private_endpoint();
        let mut listener = Listener::bind_to(&name).unwrap();
        let mut client = Pipe::connect_to(&name, &this_process()).unwrap();
        let mut server = listener.accept().unwrap();
        assert!(client.peer_open());
        assert!(client.peer_open());
        // A reply nobody asked for is not a healthy connection either.
        server.begin(Duration::from_secs(2));
        server.write_all(b"x").unwrap();
        assert!(!client.peer_open());
        client.begin(Duration::from_secs(2));
        client.read_exact(&mut [0]).unwrap();
        assert!(client.peer_open());
        drop(server);
        assert!(!client.peer_open());
        assert!(!crate::frontend::Transport::is_open(&mut client));
    }
    #[test]
    fn an_endpoint_has_one_listener() {
        let name = private_endpoint();
        let listener = Listener::bind_to(&name).unwrap();
        // ERROR_ACCESS_DENIED: the name already has its first instance.
        let second = Listener::bind_to(&name).err().unwrap();
        assert_eq!(second.raw_os_error(), Some(5));
        drop(listener);
        assert!(Listener::bind_to(&name).is_ok());
    }
    #[test]
    fn a_peer_is_checked_against_the_expected_server_image() {
        let name = private_endpoint();
        let mut listener = Listener::bind_to(&name).unwrap();
        let other = this_process().with_file_name("another-engine.exe");
        let refused = Pipe::connect_to(&name, &other).err().unwrap();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // The frontend closed its end; the listener carries on.
        assert!(matches!(listener.accept(), Err(Refusal::Peer(_))));
        let _client = Pipe::connect_to(&name, &this_process()).unwrap();
        assert!(listener.accept().is_ok());
    }
}
