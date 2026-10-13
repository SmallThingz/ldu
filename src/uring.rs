//! Bounded filesystem batches on an existing worker thread.
//!
//! The driver owns submitted paths and a duplicate of the anchored directory.
//! Unsupported kernels keep the caller's blocking implementation. A failed
//! driver distinguishes unfinished submissions from names never submitted.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const BATCH_LIMIT: usize = 64;

#[derive(Debug)]
pub(crate) enum EntryOutcome<T> {
    NotSubmitted,
    Completed(io::Result<T>),
    /// The operation was accepted, but its completion could not be recovered.
    /// Destructive operations must not be replayed from this state.
    Unknown,
}

#[derive(Debug)]
pub(crate) enum BatchOutcome<T> {
    /// No operations were submitted; the existing blocking path is available.
    Unsupported,
    Complete(Vec<EntryOutcome<T>>),
    /// The driver has been closed before this outcome is returned.
    Failed {
        entries: Vec<EntryOutcome<T>>,
        error: io::Error,
    },
}

fn empty_results<T>(len: usize) -> Vec<EntryOutcome<T>> {
    std::iter::repeat_with(|| EntryOutcome::NotSubmitted)
        .take(len)
        .collect()
}

pub(crate) fn metadata_available() -> bool {
    platform::available(platform::Operation::Metadata)
}

pub(crate) fn unlink_available() -> bool {
    platform::available(platform::Operation::Unlink)
}

pub(crate) fn metadata_batch<N: AsRef<OsStr>>(
    parent: &OwnedFd,
    names: &[N],
    cancel: &AtomicBool,
    admit: impl FnMut(usize) -> bool,
) -> BatchOutcome<rustix::fs::Stat> {
    if names.is_empty() || cancel.load(Ordering::Relaxed) {
        return BatchOutcome::Complete(empty_results(names.len()));
    }
    platform::metadata(parent, names, cancel, admit)
}

pub(crate) fn unlink_batch<N: AsRef<OsStr>>(
    parent: &OwnedFd,
    names: &[N],
    cancel: &AtomicBool,
) -> BatchOutcome<()> {
    if names.is_empty() || cancel.load(Ordering::Relaxed) {
        return BatchOutcome::Complete(empty_results(names.len()));
    }
    platform::unlink(parent, names, cancel)
}

#[cfg(any(
    test,
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
mod engine {
    use super::*;
    use std::time::Duration;

    pub(super) enum Completion<K, T> {
        Pending(K),
        Ready(io::Result<T>),
    }

    pub(super) trait Driver {
        type Operation;
        type Key;
        type Output;

        fn push(&mut self, operation: Self::Operation) -> Completion<Self::Key, Self::Output>;
        fn poll(&mut self) -> io::Result<()>;
        fn pop(&mut self, key: Self::Key) -> Completion<Self::Key, Self::Output>;
    }

    /// Keep admission and completion mapping independent of the kernel driver.
    /// Only accepted operations can become Unknown after a fatal poll failure.
    pub(super) fn run<D: Driver>(
        driver: &mut D,
        len: usize,
        cancel: &AtomicBool,
        mut admit: impl FnMut(usize) -> bool,
        mut operation: impl FnMut(usize) -> io::Result<D::Operation>,
    ) -> BatchOutcome<D::Output> {
        let mut entries = empty_results(len);
        let mut pending = Vec::with_capacity(len.min(BATCH_LIMIT));
        let mut next = 0;
        loop {
            while next < len && pending.len() < BATCH_LIMIT {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let index = next;
                next += 1;
                if !admit(index) {
                    continue;
                }
                match operation(index) {
                    Ok(operation) => match driver.push(operation) {
                        Completion::Pending(key) => pending.push((index, key)),
                        Completion::Ready(result) => {
                            entries[index] = EntryOutcome::Completed(result);
                        }
                    },
                    Err(error) => entries[index] = EntryOutcome::Completed(Err(error)),
                }
            }
            if pending.is_empty() {
                return BatchOutcome::Complete(entries);
            }

            let polled = driver.poll();
            let pending_before = pending.len();
            // A poll error need not discard results already delivered by an
            // earlier submission or poll. Preserve every known completion.
            let mut cursor = 0;
            while cursor < pending.len() {
                let (index, key) = pending.swap_remove(cursor);
                match driver.pop(key) {
                    Completion::Ready(result) => {
                        entries[index] = EntryOutcome::Completed(result);
                    }
                    Completion::Pending(key) => {
                        pending.push((index, key));
                        // swap_remove moved an unvisited item into this slot;
                        // move the still-pending item back without skipping it.
                        let last = pending.len() - 1;
                        pending.swap(cursor, last);
                        cursor += 1;
                    }
                }
            }
            if let Err(error) = polled {
                match error.kind() {
                    io::ErrorKind::TimedOut => {}
                    io::ErrorKind::Interrupted => {
                        // Compio maps EAGAIN/EBUSY to Interrupted. A repeated
                        // unsuccessful enter must not busy-spin on this worker.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    _ => {
                        for (index, _) in pending {
                            entries[index] = EntryOutcome::Unknown;
                        }
                        return BatchOutcome::Failed { entries, error };
                    }
                }
            } else if pending.len() == pending_before {
                // Spurious notifier completions can wake a poll without user
                // progress (including older kernels without multishot poll).
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
mod platform {
    use super::engine::{self, Completion, Driver};
    use super::*;
    use compio_buf::{BufResult, IntoInner};
    use compio_driver::op::{OpCodeFlag, PathStat, Unlink};
    use compio_driver::{Key, OpCode, Proactor, PushEntry};
    use io_uring::{IoUring, Probe, opcode};
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::rc::Rc;
    use std::sync::OnceLock;
    use std::time::Duration;

    #[derive(Clone, Copy)]
    pub(super) enum Operation {
        Metadata,
        Unlink,
    }

    #[derive(Clone, Copy)]
    struct Capabilities {
        metadata: bool,
        unlink: bool,
    }

    impl Capabilities {
        fn supports(self, operation: Operation) -> bool {
            match operation {
                Operation::Metadata => self.metadata,
                Operation::Unlink => self.unlink,
            }
        }
    }

    fn capabilities() -> Option<Capabilities> {
        static CAPABILITIES: OnceLock<Option<Capabilities>> = OnceLock::new();
        *CAPABILITIES.get_or_init(|| {
            let ring = IoUring::new(2).ok()?;
            let mut probe = Probe::new();
            ring.submitter().register_probe(&mut probe).ok()?;
            if !probe.is_supported(opcode::PollAdd::CODE)
                || !probe.is_supported(opcode::AsyncCancel::CODE)
            {
                return None;
            }
            Some(Capabilities {
                metadata: probe.is_supported(opcode::Statx::CODE),
                unlink: probe.is_supported(opcode::UnlinkAt::CODE),
            })
        })
    }

    struct State {
        initialized: bool,
        driver: Option<Proactor>,
    }

    thread_local! {
        static DRIVER: RefCell<State> = const {
            RefCell::new(State { initialized: false, driver: None })
        };
    }

    fn initialize_with(state: &mut State, create: impl FnOnce() -> io::Result<Proactor>) -> bool {
        if !state.initialized {
            state.initialized = true;
            state.driver = create()
                .ok()
                .filter(|driver| driver.driver_type().is_iouring());
        }
        state.driver.is_some()
    }

    fn initialize(state: &mut State) -> bool {
        initialize_with(state, || {
            // Leave space beyond the 64 data operations for the driver's
            // notifier. A full SQ can make upstream push retry internally.
            Proactor::builder()
                .driver_type(compio_driver::DriverType::IoUring)
                .capacity(128)
                .thread_pool_limit(1)
                // This primes Compio's own complete opcode probe before the
                // persistent ring consumes descriptors or locked memory.
                .detect_opcode_support(OpCodeFlag::PollAdd | OpCodeFlag::AsyncCancel)
                .build()
        })
    }

    pub(super) fn available(operation: Operation) -> bool {
        if !capabilities().is_some_and(|caps| caps.supports(operation)) {
            return false;
        }
        DRIVER
            .try_with(|slot| {
                let Ok(mut state) = slot.try_borrow_mut() else {
                    return false;
                };
                initialize(&mut state)
            })
            .unwrap_or(false)
    }

    fn with_driver<T>(
        operation: Operation,
        run: impl FnOnce(&mut Proactor) -> BatchOutcome<T>,
    ) -> BatchOutcome<T> {
        if !capabilities().is_some_and(|caps| caps.supports(operation)) {
            return BatchOutcome::Unsupported;
        }
        DRIVER
            .try_with(|slot| {
                let Ok(mut state) = slot.try_borrow_mut() else {
                    return BatchOutcome::Unsupported;
                };
                if !initialize(&mut state) {
                    return BatchOutcome::Unsupported;
                }
                let result = run(state.driver.as_mut().expect("driver was initialized"));
                if matches!(result, BatchOutcome::Failed { .. }) {
                    // Closing the ring waits for kernel access to owned paths
                    // and descriptors to end. This may block this filesystem
                    // worker; the UI never joins it during shutdown.
                    drop(state.driver.take());
                }
                result
            })
            .unwrap_or(BatchOutcome::Unsupported)
    }

    struct NativeDriver<'a, O, T> {
        driver: &'a mut Proactor,
        finish: fn(O) -> T,
    }

    impl<O: OpCode + 'static, T> NativeDriver<'_, O, T> {
        fn map(&self, entry: PushEntry<Key<O>, BufResult<usize, O>>) -> Completion<Key<O>, T> {
            match entry {
                PushEntry::Pending(key) => Completion::Pending(key),
                PushEntry::Ready(BufResult(result, operation)) => {
                    Completion::Ready(result.map(|_| (self.finish)(operation)))
                }
            }
        }
    }

    impl<O: OpCode + 'static, T> Driver for NativeDriver<'_, O, T> {
        type Operation = O;
        type Key = Key<O>;
        type Output = T;

        fn push(&mut self, operation: O) -> Completion<Key<O>, T> {
            let entry = self.driver.push(operation);
            self.map(entry)
        }

        fn poll(&mut self) -> io::Result<()> {
            self.driver.poll(Some(Duration::from_millis(10)))
        }

        fn pop(&mut self, key: Key<O>) -> Completion<Key<O>, T> {
            let entry = self.driver.pop(key);
            self.map(entry)
        }
    }

    fn basename(bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch operation requires a single basename",
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn metadata<N: AsRef<OsStr>>(
        parent: &OwnedFd,
        names: &[N],
        cancel: &AtomicBool,
        admit: impl FnMut(usize) -> bool,
    ) -> BatchOutcome<rustix::fs::Stat> {
        with_driver(Operation::Metadata, |driver| {
            let parent = match parent.try_clone() {
                Ok(parent) => Rc::new(parent),
                Err(_) => return BatchOutcome::Unsupported,
            };
            let mut driver = NativeDriver {
                driver,
                finish: PathStat::into_inner,
            };
            engine::run(&mut driver, names.len(), cancel, admit, |index| {
                let bytes = names[index].as_ref().as_bytes();
                basename(bytes)?;
                let name = CString::new(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                Ok(PathStat::new(Rc::clone(&parent), name, false))
            })
        })
    }

    pub(super) fn unlink<N: AsRef<OsStr>>(
        parent: &OwnedFd,
        names: &[N],
        cancel: &AtomicBool,
    ) -> BatchOutcome<()> {
        with_driver(Operation::Unlink, |driver| {
            let parent = match parent.try_clone() {
                Ok(parent) => Rc::new(parent),
                Err(_) => return BatchOutcome::Unsupported,
            };
            let mut driver = NativeDriver {
                driver,
                finish: drop::<Unlink<Rc<OwnedFd>>>,
            };
            engine::run(
                &mut driver,
                names.len(),
                cancel,
                |_| true,
                |index| {
                    let bytes = names[index].as_ref().as_bytes();
                    basename(bytes)?;
                    let name = CString::new(bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                    Ok(Unlink::new(Rc::clone(&parent), name, false))
                },
            )
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unavailable_driver_initialization_is_cached() {
            let mut state = State {
                initialized: false,
                driver: None,
            };
            let mut attempts = 0;
            for _ in 0..4 {
                assert!(!initialize_with(&mut state, || {
                    attempts += 1;
                    Err(io::Error::from(io::ErrorKind::Unsupported))
                }));
            }
            assert_eq!(attempts, 1);
        }

        #[test]
        fn operation_names_cannot_escape_the_held_parent() {
            for bytes in [b"".as_slice(), b".", b"..", b"a/b", b"/outside"] {
                assert!(basename(bytes).is_err());
            }
            assert!(basename(b"plain").is_ok());
            assert!(basename(b"raw-\xff").is_ok());
        }
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
mod platform {
    use super::*;

    pub(super) enum Operation {
        Metadata,
        Unlink,
    }

    pub(super) fn available(_: Operation) -> bool {
        false
    }

    pub(super) fn metadata<N: AsRef<OsStr>>(
        _: &OwnedFd,
        _: &[N],
        _: &AtomicBool,
        _: impl FnMut(usize) -> bool,
    ) -> BatchOutcome<rustix::fs::Stat> {
        BatchOutcome::Unsupported
    }

    pub(super) fn unlink<N: AsRef<OsStr>>(
        _: &OwnedFd,
        _: &[N],
        _: &AtomicBool,
    ) -> BatchOutcome<()> {
        BatchOutcome::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::engine::{self, Completion, Driver};
    use super::*;
    use std::collections::HashSet;

    struct FakeDriver<'a> {
        accepted: Vec<usize>,
        live: HashSet<usize>,
        high_water: usize,
        polls: usize,
        cancel_on_poll: Option<&'a AtomicBool>,
        fatal_on_poll: Option<usize>,
        complete_before_failure: Option<usize>,
    }

    impl FakeDriver<'_> {
        fn new() -> Self {
            Self {
                accepted: Vec::new(),
                live: HashSet::new(),
                high_water: 0,
                polls: 0,
                cancel_on_poll: None,
                fatal_on_poll: None,
                complete_before_failure: None,
            }
        }
    }

    impl Driver for FakeDriver<'_> {
        type Operation = usize;
        type Key = usize;
        type Output = usize;

        fn push(&mut self, operation: usize) -> Completion<usize, usize> {
            self.accepted.push(operation);
            assert!(self.live.insert(operation));
            self.high_water = self.high_water.max(self.live.len());
            Completion::Pending(operation)
        }

        fn poll(&mut self) -> io::Result<()> {
            self.polls += 1;
            if let Some(cancel) = self.cancel_on_poll.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            if self.fatal_on_poll == Some(self.polls) {
                Err(io::Error::other("injected driver failure"))
            } else {
                Ok(())
            }
        }

        fn pop(&mut self, key: usize) -> Completion<usize, usize> {
            if self.fatal_on_poll == Some(self.polls) && self.complete_before_failure != Some(key) {
                Completion::Pending(key)
            } else {
                assert!(self.live.remove(&key));
                Completion::Ready(Ok(key))
            }
        }
    }

    #[test]
    fn batches_are_bounded_and_out_of_order_completions_keep_input_order() {
        let mut driver = FakeDriver::new();
        let cancel = AtomicBool::new(false);
        let len = BATCH_LIMIT * 3 + 7;
        let result = engine::run(&mut driver, len, &cancel, |_| true, Ok);
        let BatchOutcome::Complete(entries) = result else {
            panic!("batch did not finish");
        };
        assert_eq!(entries.len(), len);
        assert_eq!(driver.high_water, BATCH_LIMIT);
        assert!(driver.live.is_empty());
        for (index, entry) in entries.into_iter().enumerate() {
            assert!(matches!(entry, EntryOutcome::Completed(Ok(value)) if value == index));
        }
    }

    #[test]
    fn cancellation_drains_accepted_work_and_never_admits_the_tail() {
        let cancel = AtomicBool::new(false);
        let mut driver = FakeDriver::new();
        driver.cancel_on_poll = Some(&cancel);
        let result = engine::run(&mut driver, BATCH_LIMIT + 9, &cancel, |_| true, Ok);
        let BatchOutcome::Complete(entries) = result else {
            panic!("cancelled batch did not drain");
        };
        assert_eq!(driver.accepted.len(), BATCH_LIMIT);
        assert!(driver.live.is_empty());
        assert!(
            entries[..BATCH_LIMIT]
                .iter()
                .all(|entry| matches!(entry, EntryOutcome::Completed(Ok(_))))
        );
        assert!(
            entries[BATCH_LIMIT..]
                .iter()
                .all(|entry| matches!(entry, EntryOutcome::NotSubmitted))
        );
    }

    #[test]
    fn fatal_poll_preserves_known_results_without_replaying_unknown_operations() {
        let cancel = AtomicBool::new(false);
        let mut driver = FakeDriver::new();
        driver.fatal_on_poll = Some(2);
        driver.complete_before_failure = Some(BATCH_LIMIT + 3);
        let len = BATCH_LIMIT * 2 + 9;
        let result = engine::run(&mut driver, len, &cancel, |_| true, Ok);
        let BatchOutcome::Failed { entries, error } = result else {
            panic!("driver failure disappeared");
        };
        assert_eq!(error.to_string(), "injected driver failure");
        assert_eq!(driver.accepted.len(), BATCH_LIMIT * 2);
        assert_eq!(entries.len(), len);
        for (index, entry) in entries.iter().enumerate() {
            if index < BATCH_LIMIT || index == BATCH_LIMIT + 3 {
                assert!(matches!(entry, EntryOutcome::Completed(Ok(value)) if *value == index));
            } else if index < BATCH_LIMIT * 2 {
                assert!(matches!(entry, EntryOutcome::Unknown));
            } else {
                assert!(matches!(entry, EntryOutcome::NotSubmitted));
            }
        }
    }

    #[test]
    fn filtered_names_and_pre_cancelled_batches_are_never_submitted() {
        let cancel = AtomicBool::new(false);
        let mut driver = FakeDriver::new();
        let result = engine::run(&mut driver, 8, &cancel, |index| index % 2 == 0, Ok);
        let BatchOutcome::Complete(entries) = result else {
            panic!("batch did not finish");
        };
        assert_eq!(driver.accepted, [0, 2, 4, 6]);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(matches!(entry, EntryOutcome::NotSubmitted), index % 2 != 0);
        }
        cancel.store(true, Ordering::Relaxed);
        let mut driver = FakeDriver::new();
        let result = engine::run(
            &mut driver,
            8,
            &cancel,
            |_| panic!("admitted after cancel"),
            Ok,
        );
        assert!(
            matches!(result, BatchOutcome::Complete(entries) if entries.iter().all(|entry| matches!(entry, EntryOutcome::NotSubmitted)))
        );
        assert!(driver.accepted.is_empty());
        assert_eq!(driver.polls, 0);
    }
}
