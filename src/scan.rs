use crate::backend::{self, Backend, EntryMetadata, RootAnchor};
use crate::types::{Entry, NodeId, NodeKind, ScanEvent, Sizes};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const EVENT_CAPACITY: usize = 128;
const SEND_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    pub batch_size: usize,
    pub buffer_size: usize,
    pub backend: Backend,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            threads: thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1)
                .clamp(1, 16),
            batch_size: 128,
            buffer_size: 256 * 1024,
            backend: Backend::Auto,
        }
    }
}

pub struct ScanHandle {
    pub receiver: Receiver<ScanEvent>,
    pub cancel: Arc<AtomicBool>,
    pub finished: Arc<AtomicBool>,
    pub anchor: Arc<OnceLock<Arc<RootAnchor>>>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for ScanHandle {
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

#[derive(Debug)]
struct DirectoryTask {
    id: NodeId,
    path: PathBuf,
}

struct Emitter {
    sender: Sender<ScanEvent>,
    next_id: NodeId,
    hardlinks: HashMap<(u64, u64), NodeId>,
}

impl Emitter {
    fn send(&self, mut event: ScanEvent, cancel: &AtomicBool, terminal: bool) -> io::Result<()> {
        loop {
            if !terminal && cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
            }
            match self.sender.send_timeout(event, SEND_INTERVAL) {
                Ok(()) => return Ok(()),
                Err(SendTimeoutError::Timeout(returned)) => event = returned,
                Err(SendTimeoutError::Disconnected(_)) => {
                    cancel.store(true, Ordering::Release);
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "scan event receiver disconnected",
                    ));
                }
            }
        }
    }

    fn entries(
        &mut self,
        task: &DirectoryTask,
        discovered: Vec<(std::ffi::OsString, io::Result<EntryMetadata>)>,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<DirectoryTask>> {
        let mut entries = Vec::with_capacity(discovered.len());
        let mut directories = Vec::new();
        for (name, metadata) in discovered {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
                io::Error::other("too many entries for the scan's node identifiers")
            })?;
            let (kind, sizes, hardlink_group, error) = match metadata {
                Ok(metadata) => {
                    let group = if metadata.kind == NodeKind::File {
                        metadata.identity.and_then(|identity| {
                            // A deletion can reduce a late alias to one link. Keep its
                            // existing group so the model can transfer the byte charge.
                            self.hardlinks.get(&identity).copied().or_else(|| {
                                if metadata.links > 1 {
                                    self.hardlinks.insert(identity, id);
                                    Some(id)
                                } else {
                                    None
                                }
                            })
                        })
                    } else {
                        None
                    };
                    (metadata.kind, metadata.sizes, group, None)
                }
                Err(error) => (
                    NodeKind::Unknown,
                    Sizes::default(),
                    None,
                    Some(error.to_string()),
                ),
            };
            let hardlink_duplicate = hardlink_group.is_some_and(|group| group != id);
            if kind == NodeKind::Directory {
                directories.push(DirectoryTask {
                    id,
                    path: task.path.join(&name),
                });
            }
            entries.push(Entry {
                id,
                name,
                kind,
                sizes,
                hardlink_duplicate,
                hardlink_group,
                error,
            });
        }
        // IDs and delivery share one lock, so the model receives append-only IDs in order.
        self.send(
            ScanEvent::Entries {
                parent: task.id,
                entries,
            },
            cancel,
            false,
        )?;
        Ok(directories)
    }
}

/// Starts all filesystem work in the background. Dropping the handle never joins workers.
pub fn spawn(root: PathBuf, mut options: ScanOptions) -> ScanHandle {
    options.threads = options.threads.clamp(1, 128);
    options.batch_size = options.batch_size.clamp(1, 4096);
    options.buffer_size = options.buffer_size.clamp(4096, 16 * 1024 * 1024);

    let (sender, receiver) = bounded(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let anchor = Arc::new(OnceLock::new());
    let thread_anchor = Arc::clone(&anchor);
    let thread_cancel = Arc::clone(&cancel);
    let thread_finished = Arc::clone(&finished);
    let failure_sender = sender.clone();
    let spawned = thread::Builder::new()
        .name("ldu-scan".into())
        .spawn(move || {
            let _finished = FinishedGuard(thread_finished);
            run(root, options, sender, thread_cancel, thread_anchor);
        });
    if let Err(error) = spawned {
        let _ = failure_sender.try_send(ScanEvent::RootInfo {
            kind: NodeKind::Unknown,
            sizes: Sizes::default(),
            error: Some(format!("could not start scanner: {error}")),
        });
        let _ = failure_sender.try_send(ScanEvent::DirectoryDone { id: 0, error: None });
        let _ = failure_sender.try_send(ScanEvent::Finished { cancelled: false });
        finished.store(true, Ordering::Release);
    }
    ScanHandle {
        receiver,
        cancel,
        finished,
        anchor,
    }
}

fn run(
    root: PathBuf,
    options: ScanOptions,
    sender: Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
    anchor_slot: Arc<OnceLock<Arc<RootAnchor>>>,
) {
    let emitter = Mutex::new(Emitter {
        sender,
        next_id: 1,
        hardlinks: HashMap::new(),
    });

    if !cancel.load(Ordering::Acquire) {
        let metadata = RootAnchor::open(&root).and_then(|anchor| {
            let metadata = anchor.metadata()?;
            let _ = anchor_slot.set(Arc::new(anchor));
            Ok(metadata)
        });
        let (kind, sizes, error) = match metadata {
            Ok(metadata) => (metadata.kind, metadata.sizes, None),
            Err(error) => (NodeKind::Unknown, Sizes::default(), Some(error.to_string())),
        };
        let root_sent = emitter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(ScanEvent::RootInfo { kind, sizes, error }, &cancel, false)
            .is_ok();

        if root_sent && kind == NodeKind::Directory && !cancel.load(Ordering::Acquire) {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(options.threads)
                .thread_name(|index| format!("ldu-io-{index}"))
                .build();
            match pool {
                Ok(pool) => {
                    let anchor = anchor_slot.get().expect("a directory root has an anchor");
                    let mut frontier = vec![DirectoryTask {
                        id: 0,
                        path: PathBuf::new(),
                    }];
                    while !frontier.is_empty() && !cancel.load(Ordering::Acquire) {
                        let next = Mutex::new(Vec::new());
                        // A complete parallel frontier is the barrier before the next depth.
                        pool.install(|| {
                            frontier.into_par_iter().for_each(|task| {
                                scan_directory(&task, anchor, &options, &emitter, &next, &cancel);
                            });
                        });
                        frontier = next
                            .into_inner()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                }
                Err(error) => {
                    let _ = emitter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .send(
                            ScanEvent::DirectoryDone {
                                id: 0,
                                error: Some(format!("could not start I/O workers: {error}")),
                            },
                            &cancel,
                            false,
                        );
                }
            }
        } else if root_sent && !cancel.load(Ordering::Acquire) {
            let _ = emitter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(
                    ScanEvent::DirectoryDone { id: 0, error: None },
                    &cancel,
                    false,
                );
        }
    }

    // Terminal events ignore cancellation, but receiver drop still releases backpressure.
    let _ = emitter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(
            ScanEvent::Finished {
                cancelled: cancel.load(Ordering::Acquire),
            },
            &cancel,
            true,
        );
}

fn scan_directory(
    task: &DirectoryTask,
    anchor: &RootAnchor,
    options: &ScanOptions,
    emitter: &Mutex<Emitter>,
    next: &Mutex<Vec<DirectoryTask>>,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let result = backend::enumerate_anchored(
        anchor,
        &task.path,
        options.backend,
        options.buffer_size,
        options.batch_size,
        cancel,
        |names, reader| {
            if cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
            }
            // Nested Rayon jobs use the same bounded pool, including for a single wide directory.
            let discovered = names
                .into_par_iter()
                .map(|name| {
                    let metadata = if cancel.load(Ordering::Acquire) {
                        Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
                    } else {
                        reader.metadata(&name)
                    };
                    (name, metadata)
                })
                .collect();
            let directories = emitter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entries(task, discovered, cancel)?;
            next.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend(directories);
            Ok(())
        },
    );
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let _ = emitter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(
            ScanEvent::DirectoryDone {
                id: task.id,
                error: result.err().map(|error| error.to_string()),
            },
            cancel,
            false,
        );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            fs::create_dir_all(".tmp").unwrap();
            let path = PathBuf::from(".tmp").join(format!(
                "ldu-scan-test-{}-{stamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> ScanOptions {
        ScanOptions {
            threads: 4,
            batch_size: 4,
            buffer_size: 4096,
            backend: Backend::Std,
        }
    }

    fn collect(handle: &ScanHandle) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        loop {
            let event = handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("scanner failed to send its terminal event");
            let terminal = matches!(&event, ScanEvent::Finished { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    #[test]
    fn batches_have_contiguous_ids_and_strict_breadth_first_depths() {
        let fixture = Fixture::new();
        for branch in 0..8 {
            let top = fixture.0.join(format!("branch-{branch}"));
            fs::create_dir(&top).unwrap();
            for child in 0..3 {
                let directory = top.join(format!("child-{child}"));
                fs::create_dir(&directory).unwrap();
                fs::write(directory.join("payload"), b"measured bytes").unwrap();
            }
        }
        let mut deep = fixture.0.join("deep");
        for _ in 0..12 {
            fs::create_dir(&deep).unwrap();
            fs::write(deep.join("payload"), b"deep payload").unwrap();
            deep = deep.join("next");
        }

        let handle = spawn(fixture.0.clone(), options());
        let events = collect(&handle);
        let mut depths = HashMap::from([(0, 0)]);
        let mut expected_id = 1;
        let mut last_parent_depth = 0;
        let mut saw_deep = false;
        for event in events {
            match event {
                ScanEvent::RootInfo { error, .. } => assert!(error.is_none()),
                ScanEvent::Entries { parent, entries } => {
                    let parent_depth = depths[&parent];
                    assert!(parent_depth >= last_parent_depth);
                    last_parent_depth = parent_depth;
                    saw_deep |= parent_depth >= 10;
                    for entry in entries {
                        assert_eq!(entry.id, expected_id);
                        expected_id += 1;
                        assert!(entry.error.is_none());
                        depths.insert(entry.id, parent_depth + 1);
                    }
                }
                ScanEvent::DirectoryDone { error, .. } => assert!(error.is_none()),
                ScanEvent::Finished { cancelled } => assert!(!cancelled),
            }
        }
        assert!(saw_deep);
        assert!(expected_id > 70);
    }

    #[test]
    fn late_hardlink_alias_keeps_group_after_link_count_shrinks() {
        let (sender, receiver) = bounded(2);
        let mut emitter = Emitter {
            sender,
            next_id: 1,
            hardlinks: HashMap::new(),
        };
        let cancel = AtomicBool::new(false);
        let task = DirectoryTask {
            id: 0,
            path: PathBuf::new(),
        };
        let sizes = Sizes {
            apparent: 8192,
            allocated: 8192,
        };
        for (name, links) in [("original", 2), ("survivor", 1)] {
            emitter
                .entries(
                    &task,
                    vec![(
                        name.into(),
                        Ok(EntryMetadata {
                            kind: NodeKind::File,
                            sizes,
                            identity: Some((1, 42)),
                            links,
                        }),
                    )],
                    &cancel,
                )
                .unwrap();
        }
        for expected_id in 1..=2 {
            let ScanEvent::Entries { entries, .. } = receiver.recv().unwrap() else {
                panic!("expected an entry batch");
            };
            assert_eq!(entries.len(), 1);
            let entry = &entries[0];
            assert_eq!(entry.id, expected_id);
            assert_eq!(entry.hardlink_group, Some(1));
            assert_eq!(entry.hardlink_duplicate, expected_id != 1);
            assert_eq!(entry.sizes, sizes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn regular_hardlinks_are_charged_once_and_symlink_cycles_are_leaves() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let fixture = Fixture::new();
        let original = fixture.0.join("original");
        fs::write(&original, vec![7u8; 8192]).unwrap();
        fs::hard_link(&original, fixture.0.join("linked")).unwrap();
        symlink(".", fixture.0.join("cycle")).unwrap();

        let original_metadata = fs::symlink_metadata(&original).unwrap();
        let cycle_metadata = fs::symlink_metadata(fixture.0.join("cycle")).unwrap();
        let root_metadata = fs::symlink_metadata(&fixture.0).unwrap();
        let expected_apparent =
            root_metadata.len() + original_metadata.len() + cycle_metadata.len();
        let expected_allocated =
            (root_metadata.blocks() + original_metadata.blocks() + cycle_metadata.blocks()) * 512;

        let handle = spawn(fixture.0.clone(), options());
        let mut apparent = 0;
        let mut allocated = 0;
        let mut duplicate_count = 0;
        let mut hardlink_group = None;
        let mut linked_entries = 0;
        let mut entry_count = 0;
        let mut directory_done_count = 0;
        for event in collect(&handle) {
            match event {
                ScanEvent::RootInfo { sizes, error, .. } => {
                    assert!(error.is_none());
                    apparent += sizes.apparent;
                    allocated += sizes.allocated;
                }
                ScanEvent::Entries { parent, entries } => {
                    assert_eq!(parent, 0, "the cycle symlink must not be enumerated");
                    for entry in entries {
                        entry_count += 1;
                        if !entry.hardlink_duplicate {
                            apparent += entry.sizes.apparent;
                            allocated += entry.sizes.allocated;
                        }
                        duplicate_count += usize::from(entry.hardlink_duplicate);
                        if entry.kind == NodeKind::File {
                            linked_entries += 1;
                            assert_eq!(entry.sizes.apparent, original_metadata.len());
                            assert_eq!(entry.sizes.allocated, original_metadata.blocks() * 512);
                            let group = entry.hardlink_group.expect("linked file has a group");
                            if let Some(first) = hardlink_group {
                                assert_eq!(group, first);
                            } else {
                                hardlink_group = Some(group);
                            }
                        }
                        assert!(entry.error.is_none());
                        if entry.name == "cycle" {
                            assert_eq!(entry.kind, NodeKind::Symlink);
                        }
                    }
                }
                ScanEvent::DirectoryDone { error, .. } => {
                    assert!(error.is_none());
                    directory_done_count += 1;
                }
                ScanEvent::Finished { cancelled } => assert!(!cancelled),
            }
        }
        assert_eq!(entry_count, 3);
        assert_eq!(duplicate_count, 1);
        assert_eq!(linked_entries, 2);
        assert_eq!(directory_done_count, 1);
        assert_eq!(apparent, expected_apparent);
        assert_eq!(allocated, expected_allocated);
    }

    #[test]
    fn cancellation_releases_a_full_event_channel() {
        let fixture = Fixture::new();
        for index in 0..300 {
            fs::write(fixture.0.join(format!("entry-{index}")), b"x").unwrap();
        }
        let mut scan_options = options();
        scan_options.batch_size = 1;
        let handle = spawn(fixture.0.clone(), scan_options);
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.receiver.len() < EVENT_CAPACITY {
            assert!(
                Instant::now() < deadline,
                "scanner did not fill its bounded channel"
            );
            assert!(!handle.is_finished());
            thread::sleep(Duration::from_millis(2));
        }
        handle.cancel();
        let events = collect(&handle);
        assert!(matches!(
            events.last(),
            Some(ScanEvent::Finished { cancelled: true })
        ));
    }

    #[test]
    fn inaccessible_root_reports_an_error_and_terminal_event() {
        let fixture = Fixture::new();
        let handle = spawn(fixture.0.join("missing"), options());
        let events = collect(&handle);
        assert!(matches!(
            events.first(),
            Some(ScanEvent::RootInfo {
                kind: NodeKind::Unknown,
                error: Some(_),
                ..
            })
        ));
        assert!(matches!(
            events.last(),
            Some(ScanEvent::Finished { cancelled: false })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trailing_symlink_roots_are_reported_as_leaves_by_the_scanner() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("real")).unwrap();
        fs::write(fixture.0.join("real/payload"), b"must not traverse").unwrap();
        symlink("real", fixture.0.join("alias")).unwrap();
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            for suffix in ["alias/", "alias/."] {
                let mut scan_options = options();
                scan_options.backend = backend;
                let handle = spawn(fixture.0.join(suffix), scan_options);
                let events = collect(&handle);
                assert!(matches!(
                    events.first(),
                    Some(ScanEvent::RootInfo {
                        kind: NodeKind::Symlink,
                        error: None,
                        ..
                    })
                ));
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ScanEvent::Entries { .. }))
                );
                assert!(handle.anchor.get().is_some());
            }
        }
    }
}
