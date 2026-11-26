//! Non-destructive liveness checks for standard output pipes and sockets.
//!
//! Stream servers that spend long stretches waiting on a downstream peer (such as relayed
//! `legion stream` sessions) use this module to notice when their upstream client has closed the
//! read end of stdout without writing any bytes to the stream.

/// Returns `true` when standard output is a pipe or socket whose peer can be watched for hangup.
#[must_use]
pub fn stdout_is_peer_stream() -> bool {
    #[cfg(unix)]
    {
        fd_is_peer_stream(libc::STDOUT_FILENO)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Returns `true` when the peer reading standard output has closed its end.
///
/// Always returns `false` on non-Unix platforms and whenever standard output is not a pipe or
/// socket, so callers never mistake a regular file or terminal for a disconnected client.
#[must_use]
pub fn stdout_peer_closed() -> bool {
    #[cfg(unix)]
    {
        fd_is_peer_stream(libc::STDOUT_FILENO) && fd_peer_closed(libc::STDOUT_FILENO)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn fd_is_peer_stream(fd: libc::c_int) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `fstat` writes into the valid `stat` pointer and never mutates Rust memory outside it.
    let rc = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if rc != 0 {
        return false;
    }
    // SAFETY: `fstat` succeeded, so `stat` is initialized.
    let stat = unsafe { stat.assume_init() };
    let mode = stat.st_mode & libc::S_IFMT;
    mode == libc::S_IFIFO || mode == libc::S_IFSOCK
}

#[cfg(all(unix, not(target_os = "macos")))]
#[allow(unsafe_code)]
fn fd_peer_closed(fd: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: 0,
        revents: 0,
    };
    let rc = loop {
        // SAFETY: `pfd` is a valid single-element `pollfd` array; timeout 0 returns immediately.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, 0) };
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break rc;
    };
    if rc <= 0 {
        return false;
    }
    (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn fd_peer_closed(fd: libc::c_int) -> bool {
    struct KqGuard(libc::c_int);
    impl Drop for KqGuard {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an owned kqueue descriptor created in this function.
            unsafe {
                libc::close(self.0);
            }
        }
    }

    // On macOS/XNU, `poll(2)` with `events == 0` registers neither `EVFILT_READ` nor
    // `EVFILT_WRITE` and returns 0 even after the peer has closed. Registering `EVFILT_WRITE`
    // directly with `kqueue(2)` reports `EV_EOF` as soon as the peer's read half closes
    // (`PIPE_EOF` on pipes, `SS_CANTSENDMORE` on sockets), even when the write buffer is full,
    // without falsely triggering on a client's write-only half-close (`shutdown(SHUT_WR)`).
    // SAFETY: `kqueue()` takes no pointers and returns a new fd or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        // Fallback when no fd is available for `kqueue()` (e.g. EMFILE): on macOS `poll(POLLOUT)`
        // translates `EVFILT_WRITE` `EV_EOF` to `POLLHUP` without needing an extra descriptor.
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let rc = loop {
            // SAFETY: `pfd` is a valid single-element `pollfd` array; timeout 0 returns immediately.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, 0) };
            if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break rc;
        };
        return rc > 0 && (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0;
    }
    let _guard = KqGuard(kq);

    // SAFETY: `kevent` is a plain C struct; zero-initializing covers all fields.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = fd as libc::uintptr_t;
    change.filter = libc::EVFILT_WRITE;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;

    // SAFETY: `kevent` is a plain C struct; zero-initializing covers all fields.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let zero_timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = loop {
        // SAFETY: `change`, `event`, and `zero_timeout` are valid pointers for 1-element arrays.
        let rc = unsafe {
            libc::kevent(
                kq,
                &raw const change,
                1,
                &raw mut event,
                1,
                &raw const zero_timeout,
            )
        };
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break rc;
    };
    if rc <= 0 {
        return false;
    }
    if (event.flags & libc::EV_ERROR) != 0 {
        return event.data == libc::EBADF as libc::intptr_t;
    }
    (event.flags & libc::EV_EOF) != 0
}

#[cfg(all(test, unix))]
mod tests {
    use super::{fd_is_peer_stream, fd_peer_closed};
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// Creates a `CLOEXEC` pipe so sibling test threads spawning children never inherit the read
    /// end across `execve`.
    #[allow(unsafe_code)]
    fn pipe_cloexec() -> (libc::c_int, libc::c_int) {
        let mut fds = [-1 as libc::c_int; 2];
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `fds` is a valid two-element array for `pipe2`.
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: `fds` is a valid two-element array for `pipe`.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            for fd in fds {
                // SAFETY: valid freshly opened pipe descriptor.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                    0
                );
            }
        }
        (fds[0], fds[1])
    }

    /// Waits briefly for `fd_peer_closed` to turn `true`.
    ///
    /// Even with `O_CLOEXEC` / `SOCK_CLOEXEC`, a sibling test thread inside `cargo test` that
    /// calls `fork()` (e.g. `Command::pre_exec`) temporarily holds an inherited copy of the closed
    /// peer descriptor until its child completes `execve()`, delaying final kernel release by a
    /// millisecond or two under parallel load.
    fn wait_for_peer_closed(fd: libc::c_int) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !fd_peer_closed(fd) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    #[test]
    #[allow(unsafe_code)]
    fn pipe_reports_closed_only_after_reader_drops() {
        let (read_fd, write_fd) = pipe_cloexec();
        assert!(fd_is_peer_stream(write_fd));
        assert!(!fd_peer_closed(write_fd));
        // SAFETY: `read_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(read_fd) }, 0);
        assert!(wait_for_peer_closed(write_fd));
        // SAFETY: `write_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(write_fd) }, 0);
    }

    #[test]
    #[allow(unsafe_code)]
    fn pipe_with_full_buffer_detects_closed_reader_without_false_positive() {
        let (read_fd, write_fd) = pipe_cloexec();
        // Set non-blocking on write end and fill the pipe buffer completely.
        // SAFETY: valid fd and fcntl flags.
        let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let chunk = [0x5Au8; 4096];
        loop {
            // SAFETY: valid buffer pointer and fd.
            let n = unsafe { libc::write(write_fd, chunk.as_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break;
            }
        }
        assert!(
            !fd_peer_closed(write_fd),
            "full pipe with live reader must not look closed"
        );
        // SAFETY: `read_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(read_fd) }, 0);
        assert!(
            wait_for_peer_closed(write_fd),
            "full pipe with closed reader must report closed"
        );
        // SAFETY: `write_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(write_fd) }, 0);
    }

    #[test]
    fn unix_socket_ignores_client_half_close_and_detects_full_close() {
        let (client, server) = UnixStream::pair().expect("unix stream pair");
        let server_fd = server.as_raw_fd();
        assert!(fd_is_peer_stream(server_fd));
        assert!(!fd_peer_closed(server_fd));
        // Client sending EOF on its write half (as `get` and `stat` do after the header) must not
        // look like a dead client while it is still reading stdout.
        client.shutdown(Shutdown::Write).expect("shutdown write");
        assert!(!fd_peer_closed(server_fd));
        drop(client);
        assert!(wait_for_peer_closed(server_fd));
    }

    #[test]
    fn unix_socket_with_full_buffer_detects_full_close_without_false_positive() {
        use std::io::Write;
        let (client, mut server) = UnixStream::pair().expect("unix stream pair");
        server.set_nonblocking(true).expect("set nonblocking");
        let chunk = [0xA5u8; 4096];
        while server.write(&chunk).is_ok() {}
        let server_fd = server.as_raw_fd();
        assert!(
            !fd_peer_closed(server_fd),
            "full unix socket with live client must not look closed"
        );
        client.shutdown(Shutdown::Write).expect("shutdown write");
        assert!(
            !fd_peer_closed(server_fd),
            "full unix socket after client write half-close must not look closed"
        );
        drop(client);
        assert!(
            wait_for_peer_closed(server_fd),
            "full unix socket after client drop must report closed"
        );
    }

    // Not on macOS: there a peer's shutdown of its read half is not reported to the other side
    // while any descriptor still holds the socket open; only the last close is. The close cases
    // above cover macOS.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unix_socket_shutdown_both_reports_closed_immediately_even_with_dup_fd() {
        let (client, server) = UnixStream::pair().expect("unix stream pair");
        let _dup = client.try_clone().expect("clone client");
        let server_fd = server.as_raw_fd();
        assert!(!fd_peer_closed(server_fd));
        // One call, with no half-close before it: macOS answers a full shutdown that follows a
        // write half-close with ENOTCONN. The half-close cases are covered by the tests above.
        client.shutdown(Shutdown::Both).expect("shutdown both");
        assert!(
            fd_peer_closed(server_fd),
            "explicit SHUT_RDWR must report closed synchronously even with a duplicated peer fd"
        );
    }

    #[test]
    fn regular_file_is_not_a_peer_stream() {
        let file = tempfile::tempfile().expect("tempfile");
        assert!(!fd_is_peer_stream(file.as_raw_fd()));
    }
}
