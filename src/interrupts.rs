//! Count delivered shutdown signals in the descriptor the input loop waits on.
//! Signal-hook owns the async-signal-safe writes; no helper thread is needed.

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::{SigId, low_level};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct Interrupts {
    reader: UnixStream,
    registrations: [Option<SigId>; 5],
    terminate: Arc<AtomicBool>,
}

impl Interrupts {
    pub(crate) fn new() -> io::Result<Self> {
        Self::register([SIGINT, SIGTERM, SIGHUP])
    }

    fn register(signals: [i32; 3]) -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        #[cfg(target_os = "macos")]
        rustix::net::sockopt::set_socket_nosigpipe(&writer, true)?;
        let mut interrupts = Self {
            reader,
            registrations: [None; 5],
            terminate: Arc::new(AtomicBool::new(false)),
        };
        // Publish termination intent before the wake callback. The sticky flag
        // remains visible even when several notifications coalesce or a pipe is full.
        for (slot, signal) in interrupts.registrations[..2]
            .iter_mut()
            .zip(signals[1..].iter().copied())
        {
            *slot = Some(signal_hook::flag::register(
                signal,
                Arc::clone(&interrupts.terminate),
            )?);
        }
        for (slot, signal) in interrupts.registrations[2..].iter_mut().zip(signals) {
            *slot = Some(low_level::pipe::register(signal, writer.try_clone()?)?);
        }
        Ok(interrupts)
    }

    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    pub(crate) fn termination_requested(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }

    pub(crate) fn take(&mut self) -> io::Result<bool> {
        // Consume only one signal: two rapid interrupts must reach both stages
        // of graceful shutdown, even if they arrived before the next UI frame.
        match self.reader.read(&mut [0]) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "shutdown signal notification disconnected",
            )),
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        // Keep the reader alive until every registration releases its writer.
        // This also cleans up earlier registrations if startup fails midway.
        for registration in self.registrations.iter_mut().filter_map(Option::take) {
            low_level::unregister(registration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> (Interrupts, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        (
            Interrupts {
                reader,
                registrations: [None; 5],
                terminate: Arc::new(AtomicBool::new(false)),
            },
            writer,
        )
    }

    #[test]
    fn counted_interrupts_remain_available_for_both_quit_steps() {
        let (mut interrupts, mut writer) = fixture();
        assert!(!interrupts.take().unwrap());
        assert!(!interrupts.termination_requested());
        writer.write_all(b"XX").unwrap();
        assert!(interrupts.take().unwrap());
        assert!(interrupts.take().unwrap());
        assert!(!interrupts.take().unwrap());
        writer.write_all(b"X").unwrap();
        assert!(interrupts.take().unwrap());
        assert!(!interrupts.take().unwrap());
        assert!(!interrupts.termination_requested());
    }

    #[test]
    fn closed_signal_writer_returns_an_error_instead_of_spinning() {
        let (mut interrupts, writer) = fixture();
        drop(writer);
        assert_eq!(
            interrupts.take().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn a_full_notification_socket_keeps_shutdown_requests_without_blocking() {
        let (mut interrupts, mut writer) = fixture();
        loop {
            match writer.write(&[b'X'; 4096]) {
                Ok(0) => panic!("signal socket stopped accepting bytes without an error"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("unexpected signal socket error: {error}"),
            }
        }
        assert!(interrupts.take().unwrap());
        assert!(interrupts.take().unwrap());
    }

    #[test]
    fn signal_registration_is_counted_and_owned() {
        // This test owns SIGUSR2 in this process; production uses other signals.
        let mut interrupts = Interrupts::register([signal_hook::consts::SIGUSR2; 3]).unwrap();
        assert!(!interrupts.termination_requested());
        low_level::raise(signal_hook::consts::SIGUSR2).unwrap();
        assert!(interrupts.termination_requested());
        for _ in 0..3 {
            assert!(interrupts.take().unwrap());
        }
        assert!(!interrupts.take().unwrap());
        for registration in interrupts.registrations.iter_mut().filter_map(Option::take) {
            assert!(low_level::unregister(registration));
        }
        assert_eq!(
            interrupts.take().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
