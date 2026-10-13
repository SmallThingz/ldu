use std::io;

#[cfg(unix)]
pub(crate) fn configure_read() -> io::Result<()> {
    use rustix::termios::{OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
    use std::io::IsTerminal;
    use std::os::fd::AsFd;

    let stdin = io::stdin();
    let tty = if stdin.is_terminal() {
        None
    } else {
        Some(
            std::fs::File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")?,
        )
    };
    let fd = tty
        .as_ref()
        .map_or_else(|| stdin.as_fd(), |tty| tty.as_fd());
    let mut mode = tcgetattr(fd)?;
    // Crossterm retries reads while parsing incomplete UTF-8 and escape sequences.
    // Poll controls waiting; an empty follow-up read must return immediately.
    mode.special_codes[SpecialCodeIndex::VMIN] = 0;
    mode.special_codes[SpecialCodeIndex::VTIME] = 0;
    Ok(tcsetattr(fd, OptionalActions::Now, &mode)?)
}

#[cfg(not(unix))]
pub(crate) fn configure_read() -> io::Result<()> {
    Ok(())
}

/// Combine terminal, signal, and deletion notifications without an input thread or
/// repeated timer wakeups. Crossterm still owns all terminal reads and parsing.
#[cfg(unix)]
pub(crate) struct Waiter {
    stdin: io::Stdin,
    tty: Option<std::fs::File>,
    last_source_poll: std::time::Instant,
    parser_may_have_events: bool,
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    readiness: native::Readiness,
}

#[cfg(unix)]
impl Waiter {
    pub(crate) fn new() -> io::Result<Self> {
        use std::io::IsTerminal;
        let stdin = io::stdin();
        let tty = if stdin.is_terminal() {
            None
        } else {
            Some(
                std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")?,
            )
        };
        Ok(Self {
            stdin,
            tty,
            last_source_poll: std::time::Instant::now(),
            parser_may_have_events: false,
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            readiness: native::Readiness::default(),
        })
    }

    pub(crate) fn poll(
        &mut self,
        timeout: std::time::Duration,
        completion: Option<std::os::fd::BorrowedFd<'_>>,
        interrupt: std::os::fd::BorrowedFd<'_>,
        deleting: bool,
    ) -> io::Result<bool> {
        use std::os::fd::AsFd;
        use std::time::Duration;

        // The outer crossterm reader can answer a zero-time poll. Its Unix
        // source cannot: crossterm 0.29's tty.rs skips even parser.next() when
        // the timeout is zero. After a successful read, drain that inner parser
        // with a positive timeout before waiting for more descriptor activity.
        if crossterm::event::poll(Duration::ZERO)? {
            self.parser_may_have_events = true;
            return Ok(true);
        }
        if self.parser_may_have_events && self.poll_source()? {
            return Ok(true);
        }
        // Allocation failure can remove the worker notifier. Still wait for
        // terminal/signals, but revisit active deletion without a long delay.
        let timeout = if completion.is_none() && deleting {
            timeout.min(Duration::from_millis(1))
        } else {
            timeout
        };
        let terminal = self
            .tty
            .as_ref()
            .map_or_else(|| self.stdin.as_fd(), |tty| tty.as_fd());
        // Resize notifications belong to crossterm. Probe them periodically
        // even during a continuous stream of completion wakes, without sleeping
        // for every completed job. Shutdown signals wake the combined wait.
        let signal_interval = Duration::from_millis(25);
        let until_signal_poll = signal_interval.saturating_sub(self.last_source_poll.elapsed());
        let timeout = timeout.min(until_signal_poll);
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        let terminal_ready = self
            .readiness
            .wait(terminal, completion, interrupt, timeout)?;
        #[cfg(not(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )))]
        let terminal_ready = wait_for_ready(terminal, completion, interrupt, timeout)?;
        if terminal_ready || self.last_source_poll.elapsed() >= signal_interval {
            self.poll_source()
        } else {
            Ok(false)
        }
    }

    fn poll_source(&mut self) -> io::Result<bool> {
        self.last_source_poll = std::time::Instant::now();
        // Complete input is consumed immediately. An incomplete escape/UTF-8
        // sequence gets at most this bounded wait before deletion work resumes.
        self.parser_may_have_events = crossterm::event::poll(std::time::Duration::from_millis(1))?;
        Ok(self.parser_may_have_events)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_ready(
    terminal: std::os::fd::BorrowedFd<'_>,
    completion: Option<std::os::fd::BorrowedFd<'_>>,
    interrupt: std::os::fd::BorrowedFd<'_>,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut descriptors = [
        PollFd::from_borrowed_fd(terminal, PollFlags::IN),
        PollFd::from_borrowed_fd(interrupt, PollFlags::IN),
        PollFd::from_borrowed_fd(completion.unwrap_or(interrupt), PollFlags::IN),
    ];
    let count = if completion.is_some() { 3 } else { 2 };
    let timeout = Timespec {
        tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: timeout.subsec_nanos().into(),
    };
    match poll(&mut descriptors[..count], Some(&timeout)) {
        Ok(_) => {}
        Err(rustix::io::Errno::INTR) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let closed = PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL;
    finish_ready(
        Ready {
            terminal: descriptors[0].revents().contains(PollFlags::IN),
            interrupt: descriptors[1].revents().contains(PollFlags::IN),
            completion: completion.is_some() && descriptors[2].revents().contains(PollFlags::IN),
            terminal_closed: descriptors[0].revents().intersects(closed),
            interrupt_closed: descriptors[1].revents().intersects(closed),
            completion_closed: completion.is_some() && descriptors[2].revents().intersects(closed),
        },
        completion,
        interrupt,
    )
}

#[cfg(target_os = "macos")]
fn wait_for_ready(
    terminal: std::os::fd::BorrowedFd<'_>,
    completion: Option<std::os::fd::BorrowedFd<'_>>,
    interrupt: std::os::fd::BorrowedFd<'_>,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    use filedescriptor::{POLLERR, POLLHUP, POLLIN, poll, pollfd};
    use std::os::fd::AsRawFd;

    // Darwin poll does not reliably report PTY readiness. Use the same select
    // wrapper as crossterm when kqueue cannot be used.
    let mut descriptors = [terminal, interrupt, completion.unwrap_or(interrupt)].map(|fd| pollfd {
        fd: fd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    });
    let count = if completion.is_some() { 3 } else { 2 };
    match poll(&mut descriptors[..count], Some(timeout)) {
        Ok(_) => {}
        Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error)) => {
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Err(error) => return Err(io::Error::other(error)),
    }
    let terminal_ready = descriptors[0].revents & POLLIN != 0;
    // select reports EOF as readable, without a separate hangup flag. No other
    // thread reads this raw terminal, so readable with no bytes means it closed.
    let terminal_closed = if terminal_ready {
        match rustix::io::ioctl_fionread(terminal) {
            Ok(bytes) => bytes == 0,
            Err(rustix::io::Errno::INTR) => return Ok(false),
            Err(rustix::io::Errno::IO) => true,
            Err(error) => return Err(error.into()),
        }
    } else {
        false
    };
    finish_ready(
        Ready {
            terminal: terminal_ready,
            interrupt: descriptors[1].revents & POLLIN != 0,
            completion: completion.is_some() && descriptors[2].revents & POLLIN != 0,
            terminal_closed: terminal_closed || descriptors[0].revents & (POLLERR | POLLHUP) != 0,
            interrupt_closed: descriptors[1].revents & (POLLERR | POLLHUP) != 0,
            completion_closed: completion.is_some()
                && descriptors[2].revents & (POLLERR | POLLHUP) != 0,
        },
        completion,
        interrupt,
    )
}

#[cfg(unix)]
#[derive(Clone, Copy, Default)]
struct Ready {
    terminal: bool,
    interrupt: bool,
    completion: bool,
    terminal_closed: bool,
    interrupt_closed: bool,
    completion_closed: bool,
}

#[cfg(unix)]
fn finish_ready(
    ready: Ready,
    completion: Option<std::os::fd::BorrowedFd<'_>>,
    interrupt: std::os::fd::BorrowedFd<'_>,
) -> io::Result<bool> {
    // A disconnected terminal must exit instead of generating a ready-loop.
    if ready.terminal_closed {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal input disconnected",
        ));
    }
    if ready.interrupt_closed || (ready.interrupt && rustix::io::ioctl_fionread(interrupt)? == 0) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "shutdown signal notification disconnected",
        ));
    }
    if ready.completion_closed {
        return Err(io::Error::other(
            "deletion completion notification disconnected",
        ));
    }
    if ready.completion
        && let Some(completion) = completion
    {
        drain_completion(completion)?;
    }
    // The UI consumes each interrupt separately before its next wait.
    Ok(ready.terminal)
}

#[cfg(unix)]
fn drain_completion(completion: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let mut bytes = [0; 8];
    #[cfg(not(target_os = "linux"))]
    let mut bytes = [0; 256];
    // An eventfd read drains its counter in one call. Socket notifications are
    // bounded too; kqueue rearms a ready descriptor before its next wait, and
    // poll remains level-triggered if more bytes were queued.
    for _ in 0..8 {
        match rustix::io::read(completion, &mut bytes) {
            Ok(0) => {
                return Err(io::Error::other(
                    "deletion completion notification disconnected",
                ));
            }
            Ok(_) => {
                #[cfg(target_os = "linux")]
                return Ok(());
            }
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// kqueue is a readiness mechanism, not an executor for stat or unlink. Those
// operations stay on the filesystem workers. Failure to create, register or
// wait on kqueue permanently selects the ordinary poll path for this waiter.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod native {
    use super::{Ready, finish_ready, wait_for_ready};
    use mio::{Events, Interest, Poll, Token, unix::SourceFd};
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
    use std::time::{Duration, Instant};

    const TERMINAL: Token = Token(0);
    const INTERRUPT: Token = Token(1);
    const COMPLETION: Token = Token(2);

    #[derive(Default)]
    pub(super) enum Readiness {
        #[default]
        Uninitialized,
        Native(Native),
        Fallback,
    }

    pub(super) struct Native {
        poll: Poll,
        events: Events,
        descriptors: [RawFd; 3],
    }

    impl Readiness {
        pub(super) fn wait(
            &mut self,
            terminal: BorrowedFd<'_>,
            completion: Option<BorrowedFd<'_>>,
            interrupt: BorrowedFd<'_>,
            timeout: Duration,
        ) -> io::Result<bool> {
            self.wait_with(
                terminal,
                completion,
                interrupt,
                timeout,
                Native::new,
                Native::wait,
            )
        }

        fn wait_with(
            &mut self,
            terminal: BorrowedFd<'_>,
            completion: Option<BorrowedFd<'_>>,
            interrupt: BorrowedFd<'_>,
            timeout: Duration,
            create: impl FnOnce([RawFd; 3]) -> io::Result<Native>,
            wait: impl FnOnce(&mut Native, Duration) -> io::Result<Ready>,
        ) -> io::Result<bool> {
            let started = Instant::now();
            let descriptors = [
                terminal.as_raw_fd(),
                interrupt.as_raw_fd(),
                completion.map_or(-1, |fd| fd.as_raw_fd()),
            ];
            if matches!(self, Self::Native(native) if native.descriptors != descriptors) {
                *self = Self::Uninitialized;
            }
            if matches!(self, Self::Uninitialized) {
                *self = match create(descriptors) {
                    Ok(native) => Self::Native(native),
                    Err(_) => Self::Fallback,
                };
            }
            if let Self::Native(native) = self {
                match wait(native, timeout.saturating_sub(started.elapsed())) {
                    Ok(ready) => {
                        let result = finish_ready(ready, completion, interrupt);
                        // Crossterm may keep terminal bytes buffered, and the
                        // bounded completion drain may leave notifications.
                        // Rearm only active descriptors, not on idle wakeups.
                        if native.rearm(ready).is_err() {
                            *self = Self::Fallback;
                        }
                        return result;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(false),
                    Err(_) => *self = Self::Fallback,
                }
            }
            wait_for_ready(
                terminal,
                completion,
                interrupt,
                timeout.saturating_sub(started.elapsed()),
            )
        }
    }

    impl Native {
        fn new(descriptors: [RawFd; 3]) -> io::Result<Self> {
            let poll = Poll::new()?;
            for (fd, token) in descriptors.iter().zip([TERMINAL, INTERRUPT, COMPLETION]) {
                if token == COMPLETION && *fd == -1 {
                    continue;
                }
                poll.registry()
                    .register(&mut SourceFd(fd), token, Interest::READABLE)?;
            }
            Ok(Self {
                poll,
                events: Events::with_capacity(3),
                descriptors,
            })
        }

        fn wait(&mut self, timeout: Duration) -> io::Result<Ready> {
            self.poll.poll(&mut self.events, Some(timeout))?;
            let mut ready = Ready::default();
            for event in &self.events {
                let closed = event.is_error() || event.is_read_closed();
                match event.token() {
                    TERMINAL => {
                        ready.terminal |= event.is_readable();
                        ready.terminal_closed |= closed;
                    }
                    INTERRUPT => {
                        ready.interrupt |= event.is_readable();
                        ready.interrupt_closed |= closed;
                    }
                    COMPLETION => {
                        ready.completion |= event.is_readable();
                        ready.completion_closed |= closed;
                    }
                    _ => {}
                }
            }
            Ok(ready)
        }

        fn rearm(&self, ready: Ready) -> io::Result<()> {
            for (index, token, active) in [
                (0, TERMINAL, ready.terminal && !ready.terminal_closed),
                (1, INTERRUPT, ready.interrupt && !ready.interrupt_closed),
                (2, COMPLETION, ready.completion && !ready.completion_closed),
            ] {
                if active {
                    self.poll.registry().reregister(
                        &mut SourceFd(&self.descriptors[index]),
                        token,
                        Interest::READABLE,
                    )?;
                }
            }
            Ok(())
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::completion::Completion;
        use std::io::{Read, Write};
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        #[test]
        fn native_readiness_preserves_partial_input_and_later_completion() {
            let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
            let (mut terminal, mut peer) = UnixStream::pair().unwrap();
            let completion = Completion::new().unwrap();
            let mut readiness = Readiness::default();
            peer.write_all(b"abc").unwrap();
            completion.wake().signal();
            assert!(
                readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
            assert!(
                matches!(readiness, Readiness::Native(_)),
                "kqueue did not initialize"
            );
            let mut byte = [0];
            terminal.read_exact(&mut byte).unwrap();
            assert_eq!(byte[0], b'a');
            assert!(
                readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
            let mut rest = [0; 2];
            terminal.read_exact(&mut rest).unwrap();
            assert_eq!(&rest, b"bc");
            completion.wake().signal();
            assert!(
                !readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
        }

        #[test]
        fn failed_native_setup_registration_and_wait_select_working_fallback() {
            let (interrupt, mut interrupt_writer) = UnixStream::pair().unwrap();
            interrupt.set_nonblocking(true).unwrap();
            for failure in 0..3 {
                let (terminal, mut peer) = UnixStream::pair().unwrap();
                let completion = Completion::new().unwrap();
                let mut readiness = Readiness::default();
                completion.wake().signal();
                interrupt_writer.write_all(b"XX").unwrap();
                let wait_called = std::cell::Cell::new(false);
                let ready = readiness
                    .wait_with(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO,
                        |descriptors| match failure {
                            0 => Err(io::Error::from(io::ErrorKind::Unsupported)),
                            1 => Native::new([descriptors[0], -1, descriptors[2]]),
                            _ => Native::new(descriptors),
                        },
                        |_, _| {
                            wait_called.set(true);
                            Err(io::Error::from(io::ErrorKind::Unsupported))
                        },
                    )
                    .unwrap();
                assert_eq!(wait_called.get(), failure == 2);
                assert!(!ready);
                let mut pending = [0; 2];
                assert_eq!(rustix::io::read(&interrupt, &mut pending).unwrap(), 2);
                assert_eq!(pending, *b"XX");
                assert!(matches!(readiness, Readiness::Fallback));
                peer.write_all(b"x").unwrap();
                assert!(
                    readiness
                        .wait(
                            terminal.as_fd(),
                            Some(completion.as_fd()),
                            interrupt.as_fd(),
                            Duration::ZERO
                        )
                        .unwrap()
                );
                assert!(matches!(readiness, Readiness::Fallback));
            }
        }

        #[test]
        fn interrupted_native_wait_preserves_backend_and_pending_completion() {
            let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
            let (terminal, _peer) = UnixStream::pair().unwrap();
            let completion = Completion::new().unwrap();
            let mut readiness = Readiness::default();
            completion.wake().signal();
            assert!(
                !readiness
                    .wait_with(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO,
                        Native::new,
                        |_, _| Err(io::Error::from(io::ErrorKind::Interrupted)),
                    )
                    .unwrap()
            );
            assert!(matches!(readiness, Readiness::Native(_)));
            assert!(
                !readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
            let mut byte = [0];
            assert_eq!(
                rustix::io::read(completion.as_fd(), &mut byte),
                Err(rustix::io::Errno::AGAIN)
            );
        }

        #[test]
        fn fallback_terminal_hangup_does_not_spin() {
            let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
            let (terminal, peer) = UnixStream::pair().unwrap();
            let completion = Completion::new().unwrap();
            drop(peer);
            let error = Readiness::Fallback
                .wait(
                    terminal.as_fd(),
                    Some(completion.as_fd()),
                    interrupt.as_fd(),
                    Duration::ZERO,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn native_readiness_rearms_a_bounded_partial_notification_drain() {
            let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
            let (terminal, _peer) = UnixStream::pair().unwrap();
            let (completion, mut writer) = UnixStream::pair().unwrap();
            completion.set_nonblocking(true).unwrap();
            writer.write_all(&[1; 4096]).unwrap();
            let mut readiness = Readiness::default();
            assert!(
                !readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
            assert!(matches!(readiness, Readiness::Native(_)));
            assert!(
                !readiness
                    .wait(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::ZERO
                    )
                    .unwrap()
            );
            let mut byte = [0];
            assert_eq!(
                rustix::io::read(&completion, &mut byte),
                Err(rustix::io::Errno::AGAIN)
            );
        }

        #[test]
        fn native_terminal_hangup_does_not_spin() {
            let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
            let (terminal, peer) = UnixStream::pair().unwrap();
            let completion = Completion::new().unwrap();
            drop(peer);
            let error = Readiness::default()
                .wait(
                    terminal.as_fd(),
                    Some(completion.as_fd()),
                    interrupt.as_fd(),
                    Duration::ZERO,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn native_and_fallback_preserve_signals_without_completion_notifications() {
            for fallback in [false, true] {
                let (terminal, mut peer) = UnixStream::pair().unwrap();
                let (interrupt, mut writer) = UnixStream::pair().unwrap();
                interrupt.set_nonblocking(true).unwrap();
                let mut readiness = if fallback {
                    Readiness::Fallback
                } else {
                    Readiness::default()
                };
                writer.write_all(b"XX").unwrap();
                peer.write_all(b"a").unwrap();
                assert!(
                    readiness
                        .wait(terminal.as_fd(), None, interrupt.as_fd(), Duration::ZERO)
                        .unwrap()
                );
                let mut byte = [0];
                assert_eq!(rustix::io::read(&terminal, &mut byte).unwrap(), 1);
                assert_eq!(byte[0], b'a');
                assert_eq!(rustix::io::read(&interrupt, &mut byte).unwrap(), 1);
                assert_eq!(byte[0], b'X');
                assert!(
                    !readiness
                        .wait(terminal.as_fd(), None, interrupt.as_fd(), Duration::ZERO)
                        .unwrap()
                );
                assert_eq!(rustix::io::read(&interrupt, &mut byte).unwrap(), 1);
                assert_eq!(byte[0], b'X');
                assert_eq!(
                    rustix::io::read(&interrupt, &mut byte),
                    Err(rustix::io::Errno::AGAIN)
                );
                if !fallback {
                    assert!(matches!(readiness, Readiness::Native(_)));
                }
                drop(writer);
                let error = readiness
                    .wait(terminal.as_fd(), None, interrupt.as_fd(), Duration::ZERO)
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::completion::Completion;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn completion_wakes_a_blocked_wait_and_later_notifications_remain_readable() {
        let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
        let (terminal, _peer) = UnixStream::pair().unwrap();
        let completion = Completion::new().unwrap();
        let (started, ready) = crossbeam_channel::bounded(1);
        let (finished, result) = crossbeam_channel::bounded(1);
        std::thread::scope(|scope| {
            let terminal = &terminal;
            let completion = &completion;
            let interrupt = &interrupt;
            scope.spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(wait_for_ready(
                        terminal.as_fd(),
                        Some(completion.as_fd()),
                        interrupt.as_fd(),
                        Duration::from_secs(5),
                    ))
                    .unwrap();
            });
            ready.recv().unwrap();
            completion.wake().signal();
            assert!(
                !result
                    .recv_timeout(Duration::from_secs(1))
                    .expect("completion did not wake the input wait")
                    .unwrap(),
                "completion-only wake must not request a blocking parser poll"
            );
        });
        let mut counter = [0_u8; 8];
        assert_eq!(
            rustix::io::read(completion.as_fd(), &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
        completion.wake().signal();
        wait_for_ready(
            terminal.as_fd(),
            Some(completion.as_fd()),
            interrupt.as_fd(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            rustix::io::read(completion.as_fd(), &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
    }

    #[test]
    fn coalesced_completion_preserves_simultaneous_fragmented_terminal_bytes() {
        let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
        let (mut terminal, mut peer) = UnixStream::pair().unwrap();
        let completion = Completion::new().unwrap();
        peer.write_all(b"\x1b[").unwrap();
        completion.wake().signal();
        completion.wake().signal();
        assert!(
            wait_for_ready(
                terminal.as_fd(),
                Some(completion.as_fd()),
                interrupt.as_fd(),
                Duration::ZERO
            )
            .unwrap(),
            "raw input must request a positive crossterm parser poll"
        );
        let mut prefix = [0_u8; 2];
        terminal.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, *b"\x1b[");
        let mut counter = [0_u8; 8];
        assert_eq!(
            rustix::io::read(completion.as_fd(), &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
        peer.write_all(b"A").unwrap();
        wait_for_ready(
            terminal.as_fd(),
            Some(completion.as_fd()),
            interrupt.as_fd(),
            Duration::ZERO,
        )
        .unwrap();
        terminal.read_exact(&mut prefix[..1]).unwrap();
        assert_eq!(prefix[0], b'A');
    }

    #[test]
    fn terminal_hangup_returns_an_error_instead_of_a_ready_loop() {
        let (interrupt, _interrupt_writer) = UnixStream::pair().unwrap();
        let (terminal, peer) = UnixStream::pair().unwrap();
        let completion = Completion::new().unwrap();
        drop(peer);
        let error = wait_for_ready(
            terminal.as_fd(),
            Some(completion.as_fd()),
            interrupt.as_fd(),
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn interrupts_wake_a_blocked_wait_with_or_without_a_completion_descriptor() {
        for with_completion in [false, true] {
            let (terminal, _peer) = UnixStream::pair().unwrap();
            let (interrupt, mut writer) = UnixStream::pair().unwrap();
            interrupt.set_nonblocking(true).unwrap();
            let completion = Completion::new().unwrap();
            let completion = with_completion.then(|| completion.as_fd());
            let (started, ready) = crossbeam_channel::bounded(1);
            let (finished, result) = crossbeam_channel::bounded(1);
            std::thread::scope(|scope| {
                let terminal = &terminal;
                let interrupt = &interrupt;
                scope.spawn(move || {
                    started.send(()).unwrap();
                    finished
                        .send(wait_for_ready(
                            terminal.as_fd(),
                            completion,
                            interrupt.as_fd(),
                            Duration::from_secs(5),
                        ))
                        .unwrap();
                });
                ready.recv().unwrap();
                writer.write_all(b"XX").unwrap();
                assert!(
                    !result
                        .recv_timeout(Duration::from_secs(1))
                        .expect("signal did not wake the input wait")
                        .unwrap()
                );
            });
            let mut byte = [0];
            assert_eq!(rustix::io::read(&interrupt, &mut byte).unwrap(), 1);
            assert_eq!(byte[0], b'X');
            assert_eq!(rustix::io::read(&interrupt, &mut byte).unwrap(), 1);
            assert_eq!(byte[0], b'X');
            assert_eq!(
                rustix::io::read(&interrupt, &mut byte),
                Err(rustix::io::Errno::AGAIN)
            );
            drop(writer);
            let error = wait_for_ready(
                terminal.as_fd(),
                completion,
                interrupt.as_fd(),
                Duration::ZERO,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
    }
}
