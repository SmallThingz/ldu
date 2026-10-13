//! Nonblocking worker notifications. Linux coalesces writes with eventfd; other
//! Unix systems use a socketpair so both kqueue and poll can wait on completion.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

pub(crate) struct Completion {
    #[cfg(target_os = "linux")]
    reader: Arc<rustix::fd::OwnedFd>,
    #[cfg(not(target_os = "linux"))]
    reader: std::os::unix::net::UnixStream,
    #[cfg(not(target_os = "linux"))]
    writer: Arc<std::os::unix::net::UnixStream>,
}

#[derive(Clone, Default)]
pub(crate) struct Wake {
    #[cfg(target_os = "linux")]
    fd: Option<Arc<rustix::fd::OwnedFd>>,
    #[cfg(not(target_os = "linux"))]
    fd: Option<Arc<std::os::unix::net::UnixStream>>,
}

impl Completion {
    pub(crate) fn new() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use rustix::event::{EventfdFlags, eventfd};
            Ok(Self {
                reader: Arc::new(eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)?),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            writer.set_nonblocking(true)?;
            #[cfg(target_os = "macos")]
            rustix::net::sockopt::set_socket_nosigpipe(&writer, true)?;
            Ok(Self {
                reader,
                writer: Arc::new(writer),
            })
        }
    }

    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    pub(crate) fn wake(&self) -> Wake {
        Wake {
            #[cfg(target_os = "linux")]
            fd: Some(Arc::clone(&self.reader)),
            #[cfg(not(target_os = "linux"))]
            fd: Some(Arc::clone(&self.writer)),
        }
    }
}

impl Wake {
    pub(crate) fn signal(&self) {
        let Some(fd) = &self.fd else {
            return;
        };
        #[cfg(target_os = "linux")]
        {
            let increment = 1_u64.to_ne_bytes();
            // Saturation means a notification is already pending. Never wait
            // for the UI thread or retry a full descriptor.
            while let Err(rustix::io::Errno::INTR) = rustix::io::write(fd.as_ref(), &increment) {}
        }
        #[cfg(not(target_os = "linux"))]
        {
            use std::io::Write;
            // BSD's UnixStream write uses MSG_NOSIGNAL; Darwin's writer has
            // SO_NOSIGPIPE set above. UI shutdown never signals the process.
            while let Err(error) = (&**fd).write(&[1]) {
                if error.kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    #[test]
    fn worker_notification_is_nonblocking_and_remains_readable_until_drained() {
        let completion = Completion::new().unwrap();
        let wake = completion.wake();
        wake.signal();
        wake.clone().signal();
        let fd = completion.as_fd();
        let mut descriptors = [PollFd::new(&fd, PollFlags::IN)];
        poll(&mut descriptors, Some(&Timespec::default())).unwrap();
        assert!(descriptors[0].revents().contains(PollFlags::IN));
        let mut bytes = [0; 128];
        #[cfg(target_os = "linux")]
        let bytes = &mut bytes[..8];
        #[cfg(not(target_os = "linux"))]
        let bytes = &mut bytes[..];
        assert!(rustix::io::read(fd, &mut *bytes).unwrap() > 0);
        assert_eq!(rustix::io::read(fd, bytes), Err(rustix::io::Errno::AGAIN));
        wake.signal();
        poll(&mut descriptors, Some(&Timespec::default())).unwrap();
        assert!(descriptors[0].revents().contains(PollFlags::IN));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_notification_writer_explicitly_suppresses_sigpipe() {
        let completion = Completion::new().unwrap();
        assert!(rustix::net::sockopt::socket_nosigpipe(&*completion.writer).unwrap());
    }

    #[test]
    fn absent_or_closed_receiver_does_not_block_worker_exit() {
        Wake::default().signal();
        let completion = Completion::new().unwrap();
        let wake = completion.wake();
        drop(completion);
        wake.signal();
    }
}
