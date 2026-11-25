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
    // SAFETY: `pfd` is a valid single-element `pollfd` array; timeout 0 returns immediately.
    let rc = unsafe { libc::poll(&raw mut pfd, 1, 0) };
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
        return false;
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

    #[test]
    #[allow(unsafe_code)]
    fn pipe_reports_closed_only_after_reader_drops() {
        let mut fds = [-1 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element array for `pipe`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
        assert!(fd_is_peer_stream(write_fd));
        assert!(!fd_peer_closed(write_fd));
        // SAFETY: `read_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(read_fd) }, 0);
        assert!(fd_peer_closed(write_fd));
        // SAFETY: `write_fd` is owned by this test and closed once.
        assert_eq!(unsafe { libc::close(write_fd) }, 0);
    }

    #[test]
    #[allow(unsafe_code)]
    fn pipe_with_full_buffer_detects_closed_reader_without_false_positive() {
        let mut fds = [-1 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element array for `pipe`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
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
            fd_peer_closed(write_fd),
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
        assert!(fd_peer_closed(server_fd));
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
            fd_peer_closed(server_fd),
            "full unix socket after client drop must report closed"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[allow(unsafe_code)]
    fn probe_macos_poll_and_kqueue_signals() {
        fn probe(label: &str, fd: libc::c_int) {
            let poll_one = |events: libc::c_short| -> (libc::c_int, libc::c_short) {
                let mut pfd = libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&raw mut pfd, 1, 0) };
                (rc, pfd.revents)
            };
            let kq_one = |filter: i16| -> (libc::c_int, u16, u32, isize) {
                let kq = unsafe { libc::kqueue() };
                let mut change: libc::kevent = unsafe { std::mem::zeroed() };
                change.ident = fd as libc::uintptr_t;
                change.filter = filter;
                change.flags = libc::EV_ADD | libc::EV_ONESHOT;
                let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                let ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                let rc = unsafe {
                    libc::kevent(kq, &raw const change, 1, &raw mut ev, 1, &raw const ts)
                };
                unsafe { libc::close(kq) };
                (rc, ev.flags, ev.fflags, ev.data)
            };
            let p0 = poll_one(0);
            let pout = poll_one(libc::POLLOUT);
            let pin = poll_one(libc::POLLIN);
            let kw = kq_one(libc::EVFILT_WRITE);
            let kr = kq_one(libc::EVFILT_READ);
            eprintln!(
                "[macos-probe] {label}: poll(0)={p0:?} poll(OUT)=({},{:#x}) poll(IN)=({},{:#x}) kq(W)=({},flags={:#x},ff={:#x},data={}) kq(R)=({},flags={:#x},ff={:#x},data={}) closed={}",
                pout.0,
                pout.1,
                pin.0,
                pin.1,
                kw.0,
                kw.1,
                kw.2,
                kw.3,
                kr.0,
                kr.1,
                kr.2,
                kr.3,
                fd_peer_closed(fd),
            );
        }

        let mut fds = [-1 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        probe("pipe open", fds[1]);
        unsafe { libc::close(fds[0]) };
        probe("pipe reader-closed", fds[1]);
        unsafe { libc::close(fds[1]) };

        let (client, server) = UnixStream::pair().expect("pair");
        let sfd = server.as_raw_fd();
        probe("unix open", sfd);
        client.shutdown(Shutdown::Write).expect("shut wr");
        probe("unix client-shut-wr", sfd);
        drop(client);
        probe("unix client-dropped", sfd);

        let (client2, server2) = UnixStream::pair().expect("pair2");
        client2.shutdown(Shutdown::Read).expect("shut rd");
        probe("unix client-shut-rd", server2.as_raw_fd());
        drop(client2);

        // Full-buffer pipe
        let mut bfds = [-1 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(bfds.as_mut_ptr()) }, 0);
        let fl = unsafe { libc::fcntl(bfds[1], libc::F_GETFL) };
        unsafe { libc::fcntl(bfds[1], libc::F_SETFL, fl | libc::O_NONBLOCK) };
        let chunk = [0x5Au8; 4096];
        while unsafe { libc::write(bfds[1], chunk.as_ptr().cast(), chunk.len()) } > 0 {}
        probe("pipe full open", bfds[1]);
        unsafe { libc::close(bfds[0]) };
        probe("pipe full reader-closed", bfds[1]);
        unsafe { libc::close(bfds[1]) };

        // Full-buffer unix socket
        {
            use std::io::Write;
            let (uc, mut us) = UnixStream::pair().expect("full unix");
            us.set_nonblocking(true).expect("nb");
            while us.write(&chunk).is_ok() {}
            probe("unix full open", us.as_raw_fd());
            uc.shutdown(Shutdown::Write).expect("shut wr");
            probe("unix full client-shut-wr", us.as_raw_fd());
            drop(uc);
            probe("unix full client-dropped", us.as_raw_fd());
        }

        // Loopback TCP socket
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp bind");
            let addr = listener.local_addr().expect("addr");
            let tc = std::net::TcpStream::connect(addr).expect("tcp connect");
            let (ts, _) = listener.accept().expect("tcp accept");
            probe("tcp open", ts.as_raw_fd());
            tc.shutdown(Shutdown::Write).expect("tcp shut wr");
            std::thread::sleep(std::time::Duration::from_millis(20));
            probe("tcp client-shut-wr", ts.as_raw_fd());
            drop(tc);
            std::thread::sleep(std::time::Duration::from_millis(20));
            probe("tcp client-dropped", ts.as_raw_fd());
        }
    }

    #[test]
    fn regular_file_is_not_a_peer_stream() {
        let file = tempfile::tempfile().expect("tempfile");
        assert!(!fd_is_peer_stream(file.as_raw_fd()));
    }
}
