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

#[cfg(unix)]
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
    fn regular_file_is_not_a_peer_stream() {
        let file = tempfile::tempfile().expect("tempfile");
        assert!(!fd_is_peer_stream(file.as_raw_fd()));
    }
}
