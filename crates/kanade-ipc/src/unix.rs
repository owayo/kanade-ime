use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
        io::AsRawFd,
        net::UnixStream,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn socket_path() -> io::Result<PathBuf> {
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tmp.join("kanade-ime");
    if dir.as_os_str().as_bytes().len() + 12 > 103 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(dir.join("engine.sock"))
}
pub fn secure_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}
pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if uid != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    #[cfg(target_os = "macos")]
    signature::verify(stream)?;
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if cred.uid != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
mod signature;

/// Server side of a long-lived connection: waits without limit for a frame to
/// begin, then requires the whole frame within `limit`. A stalled or
/// fragmented frame is therefore bounded while an idle connection is kept.
pub fn read_idle_frame(stream: &UnixStream, limit: Duration) -> io::Result<Option<Vec<u8>>> {
    struct Bounded<'a> {
        stream: &'a UnixStream,
        deadline: Option<Instant>,
    }
    impl Read for Bounded<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let left = match self.deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    Some(left)
                }
                None => None,
            };
            self.stream.set_read_timeout(left)?;
            self.stream.read(buffer).map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    io::ErrorKind::TimedOut.into()
                } else {
                    error
                }
            })
        }
    }
    let mut reader = Bounded {
        stream,
        deadline: None,
    };
    crate::read_idle_frame(&mut reader, |reader| {
        reader.deadline = Some(Instant::now() + limit);
        Ok(())
    })
}

/// A whole-exchange deadline, including fragmented reads and blocked writes.
pub struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}
impl DeadlineStream {
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        verify_peer(&stream)?;
        Self::new(stream, timeout)
    }
    pub fn new(stream: UnixStream, timeout: Duration) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            deadline: Instant::now() + timeout,
        })
    }
    pub fn begin(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }
    /// Whether the peer still holds its end. Reads nothing and never blocks.
    /// Unread bytes outside an exchange are not a reply to anything, so they
    /// count as a broken connection too.
    pub fn peer_open(&self) -> bool {
        let mut byte = 0u8;
        let read = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        read < 0
            && matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            )
    }
    fn wait(&self, events: i16) -> io::Result<()> {
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut fds = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            let ms = left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
            let rc = unsafe { libc::poll(&mut fds, 1, ms) };
            if rc > 0 {
                return Ok(());
            }
            if rc == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}
impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.wait(libc::POLLIN)?;
            match self.stream.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                other => return other,
            }
        }
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.wait(libc::POLLOUT)?;
            match self.stream.write(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                other => return other,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_idle_connection_outlives_the_frame_limit() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let limit = Duration::from_millis(40);
        let writer = std::thread::spawn(move || {
            // Quiet for several frame limits, then one whole frame.
            std::thread::sleep(limit * 5);
            crate::write_frame(&mut client, b"key").unwrap();
            client
        });
        let started = Instant::now();
        assert_eq!(read_idle_frame(&server, limit).unwrap().unwrap(), b"key");
        assert!(started.elapsed() >= limit * 5);
        // A second quiet period on the same connection, then an orderly close.
        let client = writer.join().unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(limit * 3);
            drop(client);
        });
        assert!(read_idle_frame(&server, limit).unwrap().is_none());
        closer.join().unwrap();
    }
    #[test]
    fn a_frame_that_stalls_after_it_began_is_bounded() {
        let limit = Duration::from_millis(40);
        for sent in [&[3u8][..], &[3, 0, 0, 0][..], &[3, 0, 0, 0, b'k'][..]] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.write_all(sent).unwrap();
            let started = Instant::now();
            assert_eq!(
                read_idle_frame(&server, limit).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert!(started.elapsed() < limit * 5);
        }
    }
    #[test]
    fn a_trickled_frame_cannot_extend_its_own_limit() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let limit = Duration::from_millis(60);
        let writer = std::thread::spawn(move || {
            // Each byte arrives inside the limit; the frame as a whole does not.
            for byte in [8u8, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8] {
                if client.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        });
        let started = Instant::now();
        assert_eq!(
            read_idle_frame(&server, limit).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        drop(server);
        writer.join().unwrap();
    }
    #[test]
    fn malformed_frames_end_the_connection_at_once() {
        for header in [[0u8; 4], ((crate::MAX_FRAME as u32) + 1).to_le_bytes()] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.write_all(&header).unwrap();
            assert_eq!(
                read_idle_frame(&server, Duration::from_secs(5))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn a_closed_peer_is_noticed_without_sending() {
        let (a, b) = UnixStream::pair().unwrap();
        let io = DeadlineStream::new(a, Duration::from_millis(5)).unwrap();
        assert!(io.peer_open());
        assert!(io.peer_open());
        drop(b);
        assert!(!io.peer_open());
    }
    #[test]
    fn stalled_peer_has_a_deadline() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut io = DeadlineStream::new(a, Duration::from_millis(5)).unwrap();
        assert_eq!(
            io.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
