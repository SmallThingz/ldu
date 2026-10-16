#[cfg(feature = "libc")]
use std::os::unix::prelude::AsRawFd;
use std::{collections::VecDeque, io, os::unix::net::UnixStream, time::Duration};

#[cfg(not(feature = "libc"))]
use rustix::fd::{AsFd, AsRawFd};

use signal_hook::low_level::pipe;

use crate::event::timeout::PollTimeout;
use crate::event::Event;
use filedescriptor::{poll, pollfd, POLLIN};

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, sys::unix::parse::parse_event, InternalEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

/// Holds a prototypical Waker and a receiver we can wait on when doing select().
#[cfg(feature = "event-stream")]
struct WakePipe {
    receiver: UnixStream,
    waker: Waker,
}

#[cfg(feature = "event-stream")]
impl WakePipe {
    fn new() -> io::Result<Self> {
        let (receiver, sender) = nonblocking_unix_pair()?;
        Ok(WakePipe {
            receiver,
            waker: Waker::new(sender),
        })
    }
}

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: FileDesc<'static>,
    winch_signal_receiver: UnixStream,
    #[cfg(feature = "event-stream")]
    wake_pipe: WakePipe,
}

fn nonblocking_unix_pair() -> io::Result<(UnixStream, UnixStream)> {
    let (receiver, sender) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((receiver, sender))
}

impl UnixInternalEventSource {
    fn input_is_idle_with(
        &self,
        probe: impl FnOnce(&mut [pollfd], Option<Duration>) -> filedescriptor::Result<usize>,
    ) -> io::Result<bool> {
        if !self.parser.internal_events.is_empty() || !self.parser.buffer.is_empty() {
            return Ok(false);
        }
        let mut fds = [pollfd {
            fd: self.tty.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        }];
        // A completed zero-time check, rather than elapsed wall time, proves
        // the descriptor boundary. Never retry an interruption within this call.
        match probe(&mut fds, Some(Duration::ZERO)) {
            Ok(_) => Ok(fds[0].revents == 0),
            Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error)) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(io::Error::new(io::ErrorKind::Other, error)),
        }
    }

    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        Ok(UnixInternalEventSource {
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty: input_fd,
            winch_signal_receiver: {
                let (receiver, sender) = nonblocking_unix_pair()?;
                // Unregistering is unnecessary because EventSource is a singleton
                #[cfg(feature = "libc")]
                pipe::register(libc::SIGWINCH, sender)?;
                #[cfg(not(feature = "libc"))]
                pipe::register(rustix::process::Signal::WINCH.as_raw(), sender)?;
                receiver
            },
            #[cfg(feature = "event-stream")]
            wake_pipe: WakePipe::new()?,
        })
    }
}

/// read_complete reads from a non-blocking file descriptor
/// until the buffer is full or it would block.
///
/// Similar to `std::io::Read::read_to_end`, except this function
/// only fills the given buffer and does not read beyond that.
fn read_complete(fd: &FileDesc, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match fd.read(buf) {
            Ok(x) => return Ok(x),
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(0),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            },
        }
    }
}

impl EventSource for UnixInternalEventSource {
    fn input_is_idle(&self) -> io::Result<bool> {
        self.input_is_idle_with(poll)
    }

    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        let timeout = PollTimeout::new(timeout);

        fn make_pollfd<F: AsRawFd>(fd: &F) -> pollfd {
            pollfd {
                fd: fd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            }
        }

        #[cfg(not(feature = "event-stream"))]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
        ];

        #[cfg(feature = "event-stream")]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
            make_pollfd(&self.wake_pipe.receiver),
        ];

        while timeout.leftover().map_or(true, |t| !t.is_zero()) {
            // check if there are buffered events from the last read
            if let Some(event) = self.parser.next() {
                return Ok(Some(event));
            }
            // A full read can leave one Escape waiting for a continuation.
            // Resolve it only after observing an empty input boundary, even
            // when the caller otherwise allows an indefinite wait.
            let pending_escape = self.parser.buffer.as_slice() == b"\x1b";
            let poll_timeout = if pending_escape {
                Some(Duration::ZERO)
            } else {
                timeout.leftover()
            };
            match poll(&mut fds, poll_timeout) {
                Err(filedescriptor::Error::Poll(e)) | Err(filedescriptor::Error::Io(e)) => {
                    match e.kind() {
                        // retry on EINTR
                        io::ErrorKind::Interrupted => continue,
                        _ => return Err(e),
                    }
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("got unexpected error while polling: {:?}", e),
                    ))
                }
                Ok(_) => (),
            };
            if pending_escape && fds[0].revents == 0 {
                let event = parse_event(&self.parser.buffer, false)?;
                self.parser.buffer.clear();
                return Ok(event);
            }
            if fds[0].revents & POLLIN != 0 {
                loop {
                    let read_count = read_complete(&self.tty, &mut self.tty_buffer)?;
                    if read_count > 0 {
                        self.parser.advance(
                            &self.tty_buffer[..read_count],
                            read_count == TTY_BUFFER_SIZE,
                        );
                    }

                    if let Some(event) = self.parser.next() {
                        return Ok(Some(event));
                    }

                    if read_count == 0 {
                        break;
                    }
                }
            }
            if fds[1].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.winch_signal_receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.winch_signal_receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}
                let new_size = crate::terminal::size()?;
                return Ok(Some(InternalEvent::Event(Event::Resize(
                    new_size.0, new_size.1,
                ))));
            }

            #[cfg(feature = "event-stream")]
            if fds[2].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.wake_pipe.receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.wake_pipe.receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}

                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }
        }
        Ok(None)
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.wake_pipe.waker.clone()
    }
}

//
// Following `Parser` structure exists for two reasons:
//
//  * mimic anes Parser interface
//  * move the advancing, parsing, ... stuff out of the `try_read` method
//
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger, because when you look at the `try_read`
            // method implementation, all events are consumed before the next TTY_BUFFER
            // is processed -> events pushed.
            internal_events: VecDeque::with_capacity(128),
        }
    }
}

impl Parser {
    fn advance(&mut self, buffer: &[u8], more: bool) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;

            self.buffer.push(*byte);

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Event can't be parsed, because we don't have enough bytes for
                    // the current sequence. Keep the buffer and process next bytes.
                }
                Err(_) => {
                    // Event can't be parsed (not enough parameters, parameter is not a number, ...).
                    // Clear the buffer and continue with another sequence.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

#[cfg(all(test, not(feature = "libc")))]
mod input_idle_tests {
    use super::*;
    use crate::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::io::Write;

    fn source() -> (UnixInternalEventSource, UnixStream, UnixStream) {
        let (input, producer) = nonblocking_unix_pair().unwrap();
        let (winch_signal_receiver, signal_producer) = nonblocking_unix_pair().unwrap();
        let source = UnixInternalEventSource {
            parser: Parser::default(),
            tty_buffer: [0; TTY_BUFFER_SIZE],
            tty: FileDesc::Owned(input.into()),
            winch_signal_receiver,
            #[cfg(feature = "event-stream")]
            wake_pipe: WakePipe::new().unwrap(),
        };
        (source, producer, signal_producer)
    }

    #[test]
    fn input_idle_preserves_descriptor_input_until_normal_read() {
        let (source, mut producer, _signal) = source();
        assert!(source.input_is_idle().unwrap());
        producer.write_all(b"\r").unwrap();
        assert!(!source.input_is_idle().unwrap());
        let mut byte = [0];
        assert_eq!(source.tty.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [b'\r']);
        assert!(source.input_is_idle().unwrap());
    }

    #[test]
    fn input_idle_checks_queued_and_partial_parser_input_before_descriptor() {
        let (mut source, _producer, _signal) = source();
        source.parser.advance(b"\r", false);
        // Ordinary zero-budget polling does not examine this queued Enter.
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
        assert!(!source.input_is_idle_with(|_, _| panic!("buffered parser must win")).unwrap());
        assert!(matches!(source.parser.next(), Some(InternalEvent::Event(Event::Key(_)))));
        assert!(source.input_is_idle().unwrap());

        source.parser.advance(b"\x1b[", true);
        assert!(!source.input_is_idle_with(|_, _| panic!("partial parser must win")).unwrap());
        source.parser.advance(b"13u", false);
        assert!(!source.input_is_idle().unwrap());
        assert!(matches!(source.parser.next(), Some(InternalEvent::Event(Event::Key(_)))));
        assert!(source.input_is_idle().unwrap());
    }

    #[test]
    fn input_idle_requires_one_successful_zero_time_descriptor_check() {
        let (source, _producer, _signal) = source();
        assert!(source.input_is_idle_with(|fds, timeout| {
            assert_eq!(fds.len(), 1);
            assert_eq!(timeout, Some(Duration::ZERO));
            Ok(0)
        }).unwrap());
        assert!(!source.input_is_idle_with(|fds, timeout| {
            assert_eq!(timeout, Some(Duration::ZERO));
            fds[0].revents = POLLIN;
            Ok(1)
        }).unwrap());
        assert!(!source.input_is_idle_with(|_, _| {
            Err(filedescriptor::Error::Poll(io::Error::from(io::ErrorKind::Interrupted)))
        }).unwrap());
        assert!(!source.input_is_idle_with(|_, _| {
            Err(filedescriptor::Error::Io(io::Error::from(io::ErrorKind::Interrupted)))
        }).unwrap());
        assert_eq!(source.input_is_idle_with(|_, _| {
            Err(filedescriptor::Error::Poll(io::Error::from(io::ErrorKind::BrokenPipe)))
        }).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    fn assert_key(source: &mut UnixInternalEventSource, code: KeyCode, modifiers: KeyModifiers) {
        assert_eq!(
            source.try_read(Some(Duration::from_secs(5))).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(code, modifiers))))
        );
    }

    fn full_read_source(
        tail: &[u8],
        continuation: &[u8],
    ) -> (UnixInternalEventSource, UnixStream, UnixStream) {
        let (mut source, mut producer, signal) = source();
        let prefix_len = TTY_BUFFER_SIZE - tail.len();
        let mut input = vec![b'a'; prefix_len];
        input.extend_from_slice(tail);
        input.extend_from_slice(continuation);
        producer.write_all(&input).unwrap();
        assert_key(&mut source, KeyCode::Char('a'), KeyModifiers::NONE);
        // Prove the source reached an exact full read with its final fragment
        // retained; an incidental short read must not satisfy this regression.
        assert_eq!(source.parser.buffer.as_slice(), tail);
        assert_eq!(source.parser.internal_events.len(), prefix_len - 1);
        for _ in 1..prefix_len {
            assert_key(&mut source, KeyCode::Char('a'), KeyModifiers::NONE);
        }
        assert!(source.parser.internal_events.is_empty());
        (source, producer, signal)
    }

    #[test]
    fn full_read_trailing_escape_resolves_without_extra_input() {
        let (mut source, mut producer, _signal) = full_read_source(b"\x1b", b"");
        assert!(!source.input_is_idle().unwrap());
        // The old source reaches this finite deadline without delivering Esc.
        // No extra input may be used to make cancellation become available.
        assert_key(&mut source, KeyCode::Esc, KeyModifiers::NONE);
        assert!(source.input_is_idle().unwrap());
        producer.write_all(b"z").unwrap();
        assert_key(&mut source, KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(source.input_is_idle().unwrap());
    }

    #[test]
    fn full_read_escape_continuations_precede_idle_resolution() {
        for (continuation, code, modifiers) in [
            (b"[A".as_slice(), KeyCode::Up, KeyModifiers::NONE),
            (b"x".as_slice(), KeyCode::Char('x'), KeyModifiers::ALT),
            (b"\xc3\xb1".as_slice(), KeyCode::Char('ñ'), KeyModifiers::ALT),
        ] {
            let mut remaining = continuation.to_vec();
            remaining.extend_from_slice(b"\rz");
            let (mut source, _producer, _signal) = full_read_source(b"\x1b", &remaining);
            assert_key(&mut source, code, modifiers);
            assert_key(&mut source, KeyCode::Enter, KeyModifiers::NONE);
            assert_key(&mut source, KeyCode::Char('z'), KeyModifiers::NONE);
            assert!(source.input_is_idle().unwrap());
        }
    }

    #[test]
    fn full_read_preserves_csi_and_utf8_fragments_until_continuation() {
        for (fragment, continuation, code, modifiers) in [
            (b"\x1b[".as_slice(), b"A".as_slice(), KeyCode::Up, KeyModifiers::NONE),
            (b"\x1b[1;".as_slice(), b"2A".as_slice(), KeyCode::Up, KeyModifiers::SHIFT),
            (b"\xc3".as_slice(), b"\xb1".as_slice(), KeyCode::Char('ñ'), KeyModifiers::NONE),
        ] {
            let (mut source, mut producer, _signal) = full_read_source(fragment, b"");
            assert_eq!(source.try_read(Some(Duration::from_millis(100))).unwrap(), None);
            assert_eq!(source.parser.buffer.as_slice(), fragment);
            assert!(!source.input_is_idle().unwrap());
            let mut remaining = continuation.to_vec();
            remaining.extend_from_slice(b"\rz");
            producer.write_all(&remaining).unwrap();
            assert_key(&mut source, code, modifiers);
            assert_key(&mut source, KeyCode::Enter, KeyModifiers::NONE);
            assert_key(&mut source, KeyCode::Char('z'), KeyModifiers::NONE);
            assert!(source.input_is_idle().unwrap());
        }
    }
}
