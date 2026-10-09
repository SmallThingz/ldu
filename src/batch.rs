//! Capture an entire confirmed selection without filesystem work on the UI thread.

use crate::backend::RootAnchor;
use crate::delete::{self, ConfirmedTarget};
use crate::types::NodeId;
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, TrySendError, bounded};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const CAPACITY: usize = 128;
const INTERVAL: Duration = Duration::from_millis(20);

enum Request {
    Entry(NodeId, PathBuf, usize),
    Finish,
    Confirm,
}

pub enum BatchEvent {
    Ready {
        count: usize,
    },
    Entry {
        node: NodeId,
        relative: PathBuf,
        target: ConfirmedTarget,
    },
    Failed(String),
    Done,
}

pub struct BatchCapture {
    requests: Sender<Request>,
    events: Receiver<BatchEvent>,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    input_finished: AtomicBool,
    confirmed: AtomicBool,
    next_ordinal: AtomicUsize,
}

impl BatchCapture {
    pub fn new(anchor: Arc<RootAnchor>) -> io::Result<Self> {
        let (requests, input) = bounded(CAPACITY);
        let (output, events) = bounded(CAPACITY);
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_finished = Arc::clone(&finished);
        let worker_ready = Arc::clone(&ready);
        let _ = thread::Builder::new()
            .name("ldu-batch-capture".into())
            .spawn(move || {
                let _finished = FinishedGuard(worker_finished);
                prepare(anchor, input, output, worker_cancel, worker_ready);
            })?;
        Ok(Self {
            requests,
            events,
            cancel,
            finished,
            ready,
            input_finished: AtomicBool::new(false),
            confirmed: AtomicBool::new(false),
            next_ordinal: AtomicUsize::new(0),
        })
    }

    /// Returns rejected ownership immediately when the worker is busy or stopped.
    pub fn try_push(&self, node: NodeId, relative: PathBuf) -> Result<(), (NodeId, PathBuf)> {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        self.try_push_ordered(node, relative, ordinal)
    }

    /// Restore displayed order on the worker while callers feed stable IDs
    /// from their persistent set without traversing the whole displayed view.
    pub fn try_push_ordered(
        &self,
        node: NodeId,
        relative: PathBuf,
        ordinal: usize,
    ) -> Result<(), (NodeId, PathBuf)> {
        if self.input_finished.load(Ordering::Relaxed)
            || self.cancel.load(Ordering::Relaxed)
            || self.is_finished()
        {
            return Err((node, relative));
        }
        match self
            .requests
            .try_send(Request::Entry(node, relative, ordinal))
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(Request::Entry(node, relative, _)))
            | Err(TrySendError::Disconnected(Request::Entry(node, relative, _))) => {
                Err((node, relative))
            }
            Err(_) => unreachable!("only entries are sent by try_push"),
        }
    }

    /// Retry on the next input-loop iteration if the bounded channel is full.
    pub fn finish_input(&self) -> bool {
        if self.input_finished.load(Ordering::Relaxed) {
            return true;
        }
        if self.cancel.load(Ordering::Relaxed) || self.is_finished() {
            return false;
        }
        if self.requests.try_send(Request::Finish).is_err() {
            return false;
        }
        self.input_finished.store(true, Ordering::Relaxed);
        true
    }

    pub fn confirm(&self) -> bool {
        if self.confirmed.load(Ordering::Relaxed) {
            return true;
        }
        if !self.ready.load(Ordering::Acquire)
            || self.cancel.load(Ordering::Relaxed)
            || self.is_finished()
        {
            return false;
        }
        if self.requests.try_send(Request::Confirm).is_err() {
            return false;
        }
        self.confirmed.store(true, Ordering::Relaxed);
        true
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn event(&self) -> Option<BatchEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for BatchCapture {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct FinishedGuard(Arc<AtomicBool>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn send(output: &Sender<BatchEvent>, mut event: BatchEvent, cancel: &AtomicBool) -> bool {
    loop {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        match output.send_timeout(event, INTERVAL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => event = returned,
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn prepare(
    anchor: Arc<RootAnchor>,
    input: Receiver<Request>,
    output: Sender<BatchEvent>,
    cancel: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
) {
    let mut entries = Vec::new();
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        match input.recv_timeout(INTERVAL) {
            Ok(Request::Entry(node, relative, ordinal)) => {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let target = match delete::capture(&anchor, &relative) {
                    Ok(target) => target,
                    Err(error) => {
                        let _ = send(
                            &output,
                            BatchEvent::Failed(format!("Could not prepare {relative:?}: {error}")),
                            &cancel,
                        );
                        let _ = send(&output, BatchEvent::Done, &cancel);
                        return;
                    }
                };
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                entries.push((ordinal, node, relative, target));
            }
            Ok(Request::Finish) => break,
            Ok(Request::Confirm) => {
                let _ = send(
                    &output,
                    BatchEvent::Failed("Batch was confirmed before preparation finished.".into()),
                    &cancel,
                );
                let _ = send(&output, BatchEvent::Done, &cancel);
                return;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
    // Display positions and legacy input ordinals are unique. Sorting here
    // keeps both whole-selection allocation and sorting off the input thread.
    entries.sort_unstable_by_key(|entry| entry.0);
    ready.store(true, Ordering::Release);
    if !send(
        &output,
        BatchEvent::Ready {
            count: entries.len(),
        },
        &cancel,
    ) {
        return;
    }
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        match input.recv_timeout(INTERVAL) {
            Ok(Request::Confirm) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
    }
    for (_, node, relative, target) in entries {
        if !send(
            &output,
            BatchEvent::Entry {
                node,
                relative,
                target,
            },
            &cancel,
        ) {
            return;
        }
    }
    let _ = send(&output, BatchEvent::Done, &cancel);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture() -> tempfile::TempDir {
        std::fs::create_dir_all(".tmp").unwrap();
        tempfile::Builder::new()
            .prefix("batch-")
            .tempdir_in(".tmp")
            .unwrap()
    }

    fn next(batch: &BatchCapture) -> BatchEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = batch.event() {
                return event;
            }
            assert!(Instant::now() < deadline, "batch event timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn stopped(batch: &BatchCapture) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !batch.is_finished() {
            assert!(Instant::now() < deadline, "batch cancellation timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn captures_all_targets_before_confirming_and_preserves_replacements() {
        let work = fixture();
        std::fs::write(work.path().join("target"), b"old").unwrap();
        let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
        let batch = BatchCapture::new(Arc::clone(&anchor)).unwrap();
        assert!(!batch.confirm());
        batch.try_push(1, "target".into()).unwrap();
        batch.try_push(2, "missing".into()).unwrap();
        assert!(batch.finish_input());
        assert!(matches!(next(&batch), BatchEvent::Ready { count: 2 }));
        assert!(batch.event().is_none(), "unconfirmed entries escaped");
        std::fs::rename(work.path().join("target"), work.path().join("original")).unwrap();
        std::fs::write(work.path().join("target"), b"replacement").unwrap();
        assert!(batch.confirm());
        let BatchEvent::Entry {
            node,
            relative,
            target,
        } = next(&batch)
        else {
            panic!("expected captured entry");
        };
        assert_eq!(node, 1);
        let worker = delete::spawn_confirmed(anchor, relative, target);
        let result = worker
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(result.outcome.is_err());
        assert_eq!(
            std::fs::read(work.path().join("target")).unwrap(),
            b"replacement"
        );
        assert!(matches!(next(&batch), BatchEvent::Entry { node: 2, .. }));
        assert!(matches!(next(&batch), BatchEvent::Done));
        stopped(&batch);
    }

    #[test]
    fn captured_display_positions_define_the_streamed_queue_order() {
        let work = fixture();
        let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
        let batch = BatchCapture::new(anchor).unwrap();
        for (node, ordinal) in [(30, 2), (10, 0), (20, 1)] {
            batch
                .try_push_ordered(node, "missing".into(), ordinal)
                .unwrap();
        }
        assert!(batch.finish_input());
        assert!(matches!(next(&batch), BatchEvent::Ready { count: 3 }));
        assert!(batch.confirm());
        for expected in [10, 20, 30] {
            assert!(matches!(next(&batch), BatchEvent::Entry { node, .. } if node == expected));
        }
        assert!(matches!(next(&batch), BatchEvent::Done));
        stopped(&batch);
    }

    #[test]
    fn failed_capture_aborts_the_entire_selection_without_streaming() {
        let work = fixture();
        std::fs::write(work.path().join("valid"), b"keep").unwrap();
        let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
        let batch = BatchCapture::new(anchor).unwrap();
        batch.try_push(1, "valid".into()).unwrap();
        batch.try_push(2, "../outside".into()).unwrap();
        let _ = batch.finish_input();
        assert!(matches!(next(&batch), BatchEvent::Failed(_)));
        assert!(matches!(next(&batch), BatchEvent::Done));
        stopped(&batch);
        assert!(!batch.confirm());
        assert_eq!(std::fs::read(work.path().join("valid")).unwrap(), b"keep");
    }

    #[test]
    fn cancellation_releases_unconfirmed_and_backpressured_selections() {
        for count in [1, CAPACITY * 2] {
            let work = fixture();
            let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
            let batch = BatchCapture::new(anchor).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            for node in 0..count {
                let mut pending = (node as NodeId, PathBuf::from("missing"));
                loop {
                    match batch.try_push(pending.0, pending.1) {
                        Ok(()) => break,
                        Err(returned) => pending = returned,
                    }
                    assert!(Instant::now() < deadline, "batch input timed out");
                    thread::sleep(Duration::from_millis(1));
                }
            }
            while !batch.finish_input() {
                assert!(Instant::now() < deadline, "batch finish timed out");
                thread::sleep(Duration::from_millis(1));
            }
            assert!(matches!(next(&batch), BatchEvent::Ready { count: ready } if ready == count));
            if count > CAPACITY {
                assert!(batch.confirm());
                while batch.events.len() < CAPACITY {
                    assert!(Instant::now() < deadline, "batch output did not fill");
                    thread::sleep(Duration::from_millis(1));
                }
            }
            batch.cancel();
            stopped(&batch);
            if count == 1 {
                assert!(!batch.confirm());
                assert!(batch.event().is_none());
            }
            assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
        }
    }
}
