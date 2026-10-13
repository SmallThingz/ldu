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

/// Combine the terminal with deletion completion without an input thread or
/// repeated timer wakeups. Crossterm still owns all terminal reads and parsing.
#[cfg(target_os = "linux")]
pub(crate) struct Waiter {
    stdin: io::Stdin,
    tty: Option<std::fs::File>,
    last_source_poll: std::time::Instant,
    parser_may_have_events: bool,
}

#[cfg(target_os = "linux")]
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
        })
    }

    pub(crate) fn poll(
        &mut self,
        timeout: std::time::Duration,
        completion: Option<std::os::fd::BorrowedFd<'_>>,
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
        let Some(completion) = completion else {
            let timeout = if deleting {
                timeout.min(Duration::from_millis(1))
            } else {
                timeout
            };
            self.last_source_poll = std::time::Instant::now();
            self.parser_may_have_events =
                crossterm::event::poll(timeout.max(Duration::from_millis(1)))?;
            return Ok(self.parser_may_have_events);
        };
        let terminal = self
            .tty
            .as_ref()
            .map_or_else(|| self.stdin.as_fd(), |tty| tty.as_fd());
        // Resize and external interrupt notifications belong to crossterm and
        // the signal handler. Probe them periodically even during a continuous
        // stream of completion wakes, without sleeping for every completed job.
        let signal_interval = Duration::from_millis(25);
        let until_signal_poll = signal_interval.saturating_sub(self.last_source_poll.elapsed());
        let terminal_ready = wait_for_ready(terminal, completion, timeout.min(until_signal_poll))?;
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

#[cfg(target_os = "linux")]
fn wait_for_ready(
    terminal: std::os::fd::BorrowedFd<'_>,
    completion: std::os::fd::BorrowedFd<'_>,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut descriptors = [
        PollFd::new(&terminal, PollFlags::IN),
        PollFd::new(&completion, PollFlags::IN),
    ];
    let timeout = Timespec {
        tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: timeout.subsec_nanos().into(),
    };
    match poll(&mut descriptors, Some(&timeout)) {
        Ok(_) => {}
        Err(rustix::io::Errno::INTR) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    // A disconnected terminal must exit instead of generating a ready-loop.
    if descriptors[0]
        .revents()
        .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
    {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal input disconnected",
        ));
    }
    if descriptors[1]
        .revents()
        .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
    {
        return Err(io::Error::other(
            "deletion completion notification disconnected",
        ));
    }
    if descriptors[1].revents().contains(PollFlags::IN) {
        // One eventfd read drains the coalesced counter. A racing later write
        // remains readable for the next wait; no terminal bytes are consumed.
        let mut counter = [0_u8; 8];
        loop {
            match rustix::io::read(completion, &mut counter) {
                Ok(_) | Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(descriptors[0].revents().contains(PollFlags::IN))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use rustix::event::{EventfdFlags, eventfd};
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn completion_wakes_a_blocked_wait_and_later_notifications_remain_readable() {
        let (terminal, _peer) = UnixStream::pair().unwrap();
        let completion = eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap();
        let (started, ready) = crossbeam_channel::bounded(1);
        let (finished, result) = crossbeam_channel::bounded(1);
        std::thread::scope(|scope| {
            let terminal = &terminal;
            let completion = &completion;
            scope.spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(wait_for_ready(
                        terminal.as_fd(),
                        completion.as_fd(),
                        Duration::from_secs(5),
                    ))
                    .unwrap();
            });
            ready.recv().unwrap();
            rustix::io::write(completion, &1_u64.to_ne_bytes()).unwrap();
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
            rustix::io::read(&completion, &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
        rustix::io::write(&completion, &1_u64.to_ne_bytes()).unwrap();
        wait_for_ready(terminal.as_fd(), completion.as_fd(), Duration::ZERO).unwrap();
        assert_eq!(
            rustix::io::read(&completion, &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
    }

    #[test]
    fn coalesced_completion_preserves_simultaneous_fragmented_terminal_bytes() {
        let (mut terminal, mut peer) = UnixStream::pair().unwrap();
        let completion = eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap();
        peer.write_all(b"\x1b[").unwrap();
        rustix::io::write(&completion, &1_u64.to_ne_bytes()).unwrap();
        rustix::io::write(&completion, &7_u64.to_ne_bytes()).unwrap();
        assert!(
            wait_for_ready(terminal.as_fd(), completion.as_fd(), Duration::ZERO).unwrap(),
            "raw input must request a positive crossterm parser poll"
        );
        let mut prefix = [0_u8; 2];
        terminal.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, *b"\x1b[");
        let mut counter = [0_u8; 8];
        assert_eq!(
            rustix::io::read(&completion, &mut counter),
            Err(rustix::io::Errno::AGAIN)
        );
        peer.write_all(b"A").unwrap();
        wait_for_ready(terminal.as_fd(), completion.as_fd(), Duration::ZERO).unwrap();
        terminal.read_exact(&mut prefix[..1]).unwrap();
        assert_eq!(prefix[0], b'A');
    }

    #[test]
    fn terminal_hangup_returns_an_error_instead_of_a_ready_loop() {
        let (terminal, peer) = UnixStream::pair().unwrap();
        let completion = eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap();
        drop(peer);
        let error =
            wait_for_ready(terminal.as_fd(), completion.as_fd(), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
