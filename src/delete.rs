//! Confirmed deletion runs outside the UI and remains rooted at an open directory.

use crate::backend::RootAnchor;
use crate::types::Sizes;
use crossbeam_channel::{Receiver, bounded};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

/// Identity captured before confirming or queueing an entry. This stores no
/// descriptors, so large batches do not exhaust the process's open-file limit.
#[derive(Clone, Copy, Debug)]
pub struct ConfirmedTarget {
    #[cfg(target_os = "linux")]
    parent: Option<FileIdentity>,
    #[cfg(target_os = "linux")]
    target: Option<FileIdentity>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    kind: rustix::fs::FileType,
}

#[cfg(target_os = "linux")]
impl FileIdentity {
    #[allow(clippy::unnecessary_cast)] // Linux Stat widths differ by target.
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            kind: rustix::fs::FileType::from_raw_mode(stat.st_mode),
        }
    }
}

/// Filesystem work belongs on a worker, including this confirmation snapshot.
/// A missing snapshot never authorizes deleting a later replacement. Identity
/// checks cannot prevent a concurrent inode-reuse race after this returns.
pub fn capture(anchor: &RootAnchor, relative: &Path) -> io::Result<ConfirmedTarget> {
    let relative = validated_relative(relative)?;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{AtFlags, fstat, statat};
        let parent = match anchor.open_parent(&relative) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ConfirmedTarget {
                    parent: None,
                    target: None,
                });
            }
            Err(error) => return Err(error),
        };
        let state = WorkerState::default();
        let parent_identity = FileIdentity::from_stat(&retry_operation(&state, || fstat(&parent))?);
        let target = match retry_operation(&state, || {
            statat(
                &parent,
                relative.file_name().unwrap(),
                AtFlags::SYMLINK_NOFOLLOW,
            )
        }) {
            Ok(stat) => Some(FileIdentity::from_stat(&stat)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(ConfirmedTarget {
            parent: Some(parent_identity),
            target,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (anchor, relative);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "anchored deletion requires Linux and /proc/self/fd",
        ))
    }
}

pub struct ParentMetadata {
    /// Empty means the scan root; otherwise relative to its retained anchor.
    pub relative: PathBuf,
    pub sizes: Sizes,
}

/// Constant-time progress; removed_entries includes removed directories.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeleteProgress {
    pub examined_entries: u64,
    pub removed_entries: u64,
    pub removed_directories: u64,
}

#[derive(Default)]
struct WorkerState {
    cancel: AtomicBool,
    examined_entries: AtomicU64,
    removed_entries: AtomicU64,
    removed_directories: AtomicU64,
    #[cfg(test)]
    cancel_after: AtomicU64,
}

impl WorkerState {
    fn snapshot(&self) -> DeleteProgress {
        // Read the later counters first so a concurrent snapshot cannot show
        // more removed directories than entries, or more removals than reads.
        let removed_directories = self.removed_directories.load(Ordering::Acquire);
        let removed_entries = self.removed_entries.load(Ordering::Acquire);
        let examined_entries = self.examined_entries.load(Ordering::Relaxed);
        DeleteProgress {
            examined_entries,
            removed_entries,
            removed_directories,
        }
    }

    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "deletion cancelled",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn removed(&self, directory: bool) {
        let _ = self.removed_entries.fetch_add(1, Ordering::Release);
        if directory {
            self.removed_directories.fetch_add(1, Ordering::Release);
        }
        // A deterministic syscall-boundary cancellation fixture. Production
        // workers have no hook and use the same cancellation checks.
        #[cfg(test)]
        {
            let limit = self.cancel_after.load(Ordering::Relaxed);
            if limit != 0 && self.removed_entries.load(Ordering::Relaxed) >= limit {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
}

pub struct DeleteResult {
    pub relative: PathBuf,
    /// Interrupted means cancellation, possibly after the progress removals.
    pub outcome: io::Result<()>,
    pub progress: DeleteProgress,
    pub parent_metadata: Option<ParentMetadata>,
    /// Parent metadata failure does not change a successful deletion outcome.
    pub metadata_error: Option<String>,
}

struct Removal {
    outcome: io::Result<()>,
    parent_metadata: Option<ParentMetadata>,
    metadata_error: Option<String>,
}

impl Removal {
    fn failed(error: io::Error) -> Self {
        Self {
            outcome: Err(error),
            parent_metadata: None,
            metadata_error: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn already_absent() -> Self {
        Self {
            outcome: Ok(()),
            parent_metadata: None,
            metadata_error: None,
        }
    }
}

pub struct DeleteHandle {
    pub receiver: Receiver<DeleteResult>,
    pub finished: Arc<AtomicBool>,
    state: Arc<WorkerState>,
}

impl DeleteHandle {
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Cooperative cancellation never waits for or joins the worker.
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Relaxed);
    }

    pub fn progress(&self) -> DeleteProgress {
        self.state.snapshot()
    }
}

struct FinishedGuard(Arc<AtomicBool>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Call only after user confirmation. Dropping the handle never joins the worker.
/// The caller keeps at most one operation active and applies its result to the model.
pub fn spawn(anchor: Arc<RootAnchor>, relative: PathBuf) -> DeleteHandle {
    spawn_inner(anchor, relative, None)
}

/// Start an operation authorized by an earlier confirmation snapshot. Replacement
/// targets or parents fail without deleting the replacement object.
pub fn spawn_confirmed(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: ConfirmedTarget,
) -> DeleteHandle {
    spawn_inner(anchor, relative, Some(target))
}

fn spawn_inner(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: Option<ConfirmedTarget>,
) -> DeleteHandle {
    let (sender, receiver) = bounded(1);
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    let state = Arc::new(WorkerState::default());
    let worker_state = Arc::clone(&state);
    let failure_sender = sender.clone();
    let failure_relative = relative.clone();
    let result = thread::Builder::new()
        .name("ldu-delete".into())
        .spawn(move || {
            let _finished = FinishedGuard(worker_finished);
            let removal = remove_expected(&anchor, &relative, &worker_state, target)
                .unwrap_or_else(Removal::failed);
            let _ = sender.try_send(DeleteResult {
                relative,
                outcome: removal.outcome,
                progress: worker_state.snapshot(),
                parent_metadata: removal.parent_metadata,
                metadata_error: removal.metadata_error,
            });
        });
    if let Err(error) = result {
        let _ = failure_sender.try_send(DeleteResult {
            relative: failure_relative,
            outcome: Err(io::Error::new(
                error.kind(),
                format!("could not start deletion worker: {error}"),
            )),
            progress: state.snapshot(),
            parent_metadata: None,
            metadata_error: None,
        });
        finished.store(true, Ordering::Release);
    }
    DeleteHandle {
        receiver,
        finished,
        state,
    }
}

fn validated_relative(relative: &Path) -> io::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "deletion requires a non-root path below the scan root",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the scan root cannot be deleted",
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
fn remove_confirmed(
    anchor: &RootAnchor,
    relative: &Path,
    state: &WorkerState,
) -> io::Result<Removal> {
    remove_expected(anchor, relative, state, None)
}

fn remove_expected(
    anchor: &RootAnchor,
    relative: &Path,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
) -> io::Result<Removal> {
    let relative = validated_relative(relative)?;
    state.check_cancel()?;
    #[cfg(target_os = "linux")]
    {
        remove_linux(anchor, &relative, state, expected)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (anchor, relative, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "anchored deletion requires Linux and /proc/self/fd",
        ))
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Linux targets.
fn remove_linux(
    anchor: &RootAnchor,
    relative: &Path,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
) -> io::Result<Removal> {
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let parent = match anchor.open_parent(relative) {
        Ok(parent) => parent,
        // Another queued deletion may have removed this target's parent.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Removal::already_absent());
        }
        Err(error) => return Err(error),
    };
    state.check_cancel()?;
    let parent_path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let held = rustix::fs::fstat(&parent).map_err(io::Error::from)?;
    if expected.is_some_and(|expected| expected.parent != Some(FileIdentity::from_stat(&held))) {
        return Err(changed_target());
    }
    let visible = fs::metadata(&parent_path)?;
    if !visible.is_dir()
        || visible.dev() != held.st_dev as u64
        || visible.ino() != held.st_ino as u64
    {
        return Err(io::Error::other(
            "the anchored parent is unavailable through /proc/self/fd",
        ));
    }
    let basename = relative.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "deletion target has no basename",
        )
    })?;
    let outcome = remove_from_parent(&parent, basename, state, expected);
    // Retain parent metadata even after cooperative cancellation or partial
    // failure. A scoped refresh reconciles surviving descendants in the model.
    let (parent_metadata, metadata_error) = match rustix::fs::fstat(&parent) {
        Ok(stat) => (
            Some(ParentMetadata {
                relative: relative.parent().unwrap_or(Path::new("")).to_path_buf(),
                sizes: Sizes {
                    apparent: u64::try_from(stat.st_size).unwrap_or(0),
                    allocated: u64::try_from(stat.st_blocks)
                        .unwrap_or(0)
                        .saturating_mul(512),
                },
            }),
            None,
        ),
        Err(error) => (
            None,
            Some(format!("could not refresh parent metadata: {error}")),
        ),
    };
    Ok(Removal {
        outcome,
        parent_metadata,
        metadata_error,
    })
}

#[cfg(target_os = "linux")]
fn retry_operation<T>(
    state: &WorkerState,
    mut operation: impl FnMut() -> rustix::io::Result<T>,
) -> io::Result<T> {
    loop {
        state.check_cancel()?;
        match operation() {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result.map_err(io::Error::from),
        }
    }
}

#[cfg(target_os = "linux")]
fn identity_matches(a: &rustix::fs::Stat, b: &rustix::fs::Stat) -> bool {
    a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && rustix::fs::FileType::from_raw_mode(a.st_mode)
            == rustix::fs::FileType::from_raw_mode(b.st_mode)
}

#[cfg(target_os = "linux")]
fn changed_target() -> io::Error {
    io::Error::other("deletion target changed before removal; refresh and confirm again")
}

#[cfg(target_os = "linux")]
fn checked_current(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    captured: &rustix::fs::Stat,
    state: &WorkerState,
) -> io::Result<bool> {
    use rustix::fs::{AtFlags, statat};
    match retry_operation(state, || statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(current) if identity_matches(captured, &current) => Ok(true),
        Ok(_) => Err(changed_target()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
struct DirectoryFrame {
    fd: rustix::fd::OwnedFd,
    name: std::ffi::OsString,
    captured: rustix::fs::Stat,
    pending: Vec<DirectoryName>,
    exhausted: bool,
}

#[cfg(target_os = "linux")]
struct DirectoryName {
    name: std::ffi::OsString,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn open_checked_directory(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected: Option<&rustix::fs::Stat>,
    inode: u64,
    state: &WorkerState,
) -> io::Result<Option<DirectoryFrame>> {
    use rustix::fs::{Mode, OFlags, fstat, openat};
    let fd = match retry_operation(state, || {
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
    }) {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let captured = retry_operation(state, || fstat(&fd))?;
    if expected.is_some_and(|expected| !identity_matches(expected, &captured))
        || (inode != 0 && inode != captured.st_ino)
    {
        return Err(changed_target());
    }
    if !checked_current(parent, name, &captured, state)? {
        return Ok(None);
    }
    Ok(Some(DirectoryFrame {
        fd,
        name: name.to_owned(),
        captured,
        pending: Vec::new(),
        exhausted: false,
    }))
}

#[cfg(target_os = "linux")]
fn unlink_entry(
    parent: &rustix::fd::OwnedFd,
    name: impl rustix::path::Arg + Copy,
    directory: bool,
    state: &WorkerState,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, unlinkat};
    let flags = if directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    match retry_operation(state, || unlinkat(parent, name, flags)) {
        Ok(()) => {
            state.removed(directory);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn remove_from_parent(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, statat};
    let captured = match retry_operation(state, || statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
    {
        Ok(captured) => captured,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    state.examined_entries.fetch_add(1, Ordering::Relaxed);
    if expected.is_some_and(|expected| expected.target != Some(FileIdentity::from_stat(&captured)))
    {
        return Err(changed_target());
    }
    if !checked_current(parent, name, &captured, state)? {
        return Ok(());
    }
    if FileType::from_raw_mode(captured.st_mode) != FileType::Directory {
        return unlink_entry(parent, name, false, state);
    }
    let Some(root) = open_checked_directory(parent, name, Some(&captured), 0, state)? else {
        return Ok(());
    };
    let mut stack = vec![root];
    let mut buffer = Vec::<u8>::new();
    buffer
        .try_reserve_exact(64 * 1024)
        .map_err(io::Error::other)?;
    while !stack.is_empty() {
        state.check_cancel()?;
        if let Some(next) = stack.last_mut().unwrap().pending.pop() {
            let frame = stack.last().unwrap();
            if let Some(child) =
                open_checked_directory(&frame.fd, &next.name, None, next.inode, state)?
            {
                stack.push(child);
            }
            continue;
        }
        let frame = stack.last_mut().unwrap();
        if !frame.exhausted {
            let (pending, exhausted) = read_directory_batch(&frame.fd, &mut buffer, state)?;
            frame.pending = pending;
            frame.exhausted = exhausted;
            continue;
        }
        let finished = stack.pop().unwrap();
        let parent = stack.last().map(|frame| &frame.fd).unwrap_or(parent);
        if checked_current(parent, &finished.name, &finished.captured, state)? {
            unlink_entry(parent, finished.name.as_os_str(), true, state)?;
        }
    }
    Ok(())
}

/// Consume one whole getdents buffer before suspending its descriptor. This
/// preserves every entry without a self-referential iterator or a recursive
/// Rust call stack. Ordinary files are unlinked directly from borrowed names.
#[cfg(target_os = "linux")]
fn read_directory_batch(
    fd: &rustix::fd::OwnedFd,
    buffer: &mut Vec<u8>,
    state: &WorkerState,
) -> io::Result<(Vec<DirectoryName>, bool)> {
    use rustix::fs::{AtFlags, FileType, RawDir, statat};
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;

    let mut pending = Vec::new();
    loop {
        let capacity = buffer.capacity();
        {
            let mut entries = RawDir::new(fd, buffer.spare_capacity_mut());
            loop {
                state.check_cancel()?;
                let Some(entry) = entries.next() else {
                    return Ok((pending, true));
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(Errno::INTR) => continue,
                    Err(Errno::INVAL) if capacity < 1024 * 1024 => break,
                    Err(error) => return Err(error.into()),
                };
                let bytes = entry.file_name().to_bytes();
                if bytes != b"." && bytes != b".." {
                    state.examined_entries.fetch_add(1, Ordering::Relaxed);
                    let kind = entry.file_type();
                    let name = std::ffi::OsStr::from_bytes(bytes);
                    let is_directory = if kind == FileType::Directory {
                        true
                    } else {
                        match unlink_entry(fd, entry.file_name(), false, state) {
                            Ok(()) => false,
                            Err(error)
                                if kind == FileType::Unknown
                                    && matches!(error.raw_os_error(), Some(code)
                                        if code == Errno::ISDIR.raw_os_error()
                                            || code == Errno::PERM.raw_os_error()) =>
                            {
                                match retry_operation(state, || {
                                    statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
                                }) {
                                    Ok(stat)
                                        if FileType::from_raw_mode(stat.st_mode)
                                            == FileType::Directory =>
                                    {
                                        true
                                    }
                                    Ok(_) => return Err(error),
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                                    Err(error) => return Err(error),
                                }
                            }
                            Err(error) => return Err(error),
                        }
                    };
                    if is_directory {
                        pending.push(DirectoryName {
                            name: name.to_owned(),
                            inode: entry.ino(),
                        });
                    }
                }
                if entries.is_buffer_empty() {
                    return Ok((pending, false));
                }
            }
        }
        let new_capacity = capacity.saturating_mul(2).min(1024 * 1024);
        buffer
            .try_reserve_exact(new_capacity)
            .map_err(io::Error::other)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_escaping_paths_cannot_start_removal() {
        for relative in ["", ".", "..", "../target", "directory/../target", "/target"] {
            assert!(
                validated_relative(Path::new(relative)).is_err(),
                "accepted forbidden deletion target {relative:?}",
            );
        }
        assert_eq!(
            validated_relative(Path::new("directory/target")).unwrap(),
            PathBuf::from("directory/target"),
        );
    }

    #[cfg(target_os = "linux")]
    fn fixture() -> tempfile::TempDir {
        std::fs::create_dir_all(".tmp").unwrap();
        tempfile::Builder::new()
            .prefix("delete-")
            .tempdir_in(".tmp")
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_between_unlinks_retains_progress_and_parent_metadata() {
        let work = fixture();
        std::fs::create_dir(work.path().join("target")).unwrap();
        for n in 0..8 {
            std::fs::write(work.path().join("target").join(format!("file-{n}")), b"x").unwrap();
        }
        let anchor = RootAnchor::open(work.path()).unwrap();
        let state = WorkerState::default();
        state.cancel_after.store(2, Ordering::Relaxed);
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert_eq!(
            removal.outcome.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(state.snapshot().removed_entries, 2);
        assert_eq!(state.snapshot().removed_directories, 0);
        assert_eq!(
            std::fs::read_dir(work.path().join("target"))
                .unwrap()
                .count(),
            6
        );
        assert!(
            removal
                .parent_metadata
                .unwrap()
                .relative
                .as_os_str()
                .is_empty()
        );
        assert!(removal.metadata_error.is_none());
        let resumed = WorkerState::default();
        remove_confirmed(&anchor, Path::new("target"), &resumed)
            .unwrap()
            .outcome
            .unwrap();
        assert!(!work.path().join("target").exists());
        assert_eq!(resumed.snapshot().removed_entries, 7);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_before_start_keeps_the_target() {
        let work = fixture();
        std::fs::write(work.path().join("target"), b"keep").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let state = WorkerState::default();
        state.cancel.store(true, Ordering::Relaxed);
        let error = match remove_confirmed(&anchor, Path::new("target"), &state) {
            Ok(_) => panic!("pre-cancelled deletion succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(std::fs::read(work.path().join("target")).unwrap(), b"keep");
        assert_eq!(state.snapshot(), DeleteProgress::default());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_then_parent_and_missing_queued_targets_succeed() {
        let work = fixture();
        std::fs::create_dir_all(work.path().join("target/child")).unwrap();
        std::fs::write(work.path().join("target/child/leaf"), b"x").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        for relative in ["target/child", "target", "target/child", "target"] {
            remove_confirmed(&anchor, Path::new(relative), &WorkerState::default())
                .unwrap()
                .outcome
                .unwrap();
        }
        assert!(work.path().is_dir());
        assert!(!work.path().join("target").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn buffered_tree_removal_preserves_raw_names_and_never_follows_links() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let work = fixture();
        std::fs::create_dir_all(work.path().join("target/child/deeper")).unwrap();
        std::fs::create_dir(work.path().join("outside")).unwrap();
        std::fs::write(work.path().join("outside/sentinel"), b"keep").unwrap();
        let raw = std::ffi::OsString::from_vec(vec![b'x', 0xff]);
        std::fs::write(work.path().join("target/child/deeper").join(raw), b"x").unwrap();
        symlink(
            "../../../outside",
            work.path().join("target/child/deeper/link"),
        )
        .unwrap();
        // Several getdents buffers ensure suspending a parent never drops the
        // unread portion of its kernel batch.
        for n in 0..2200 {
            std::fs::create_dir(work.path().join("target").join(format!("dir-{n:04}"))).unwrap();
        }
        let anchor = RootAnchor::open(work.path()).unwrap();
        let state = WorkerState::default();
        remove_confirmed(&anchor, Path::new("target"), &state)
            .unwrap()
            .outcome
            .unwrap();
        assert_eq!(
            std::fs::read(work.path().join("outside/sentinel")).unwrap(),
            b"keep"
        );
        assert!(!work.path().join("target").exists());
        assert_eq!(state.snapshot().removed_directories, 2203);
        assert_eq!(state.snapshot().removed_entries, 2205);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmation_rejects_queued_target_parent_and_missing_replacements() {
        use std::os::unix::fs::symlink;
        for kind in ["file", "directory", "symlink", "missing", "parent"] {
            let work = fixture();
            std::fs::create_dir(work.path().join("parent")).unwrap();
            std::fs::write(work.path().join("outside"), b"keep").unwrap();
            let relative = Path::new("parent/target");
            let path = work.path().join(relative);
            match kind {
                "directory" => std::fs::create_dir(&path).unwrap(),
                "symlink" => symlink("../outside", &path).unwrap(),
                "missing" => {}
                _ => std::fs::write(&path, b"old").unwrap(),
            }
            let anchor = RootAnchor::open(work.path()).unwrap();
            let expected = capture(&anchor, relative).unwrap();
            if kind == "parent" {
                std::fs::rename(
                    work.path().join("parent"),
                    work.path().join("original-parent"),
                )
                .unwrap();
                std::fs::create_dir(work.path().join("parent")).unwrap();
            } else if kind != "missing" {
                std::fs::rename(&path, work.path().join("original-target")).unwrap();
            }
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("sentinel"), b"replacement").unwrap();
            let state = WorkerState::default();
            let result = remove_expected(&anchor, relative, &state, Some(expected));
            let error = match result {
                Ok(removal) => removal.outcome.unwrap_err(),
                Err(error) => error,
            };
            assert!(error.to_string().contains("changed"), "{kind}: {error}");
            assert_eq!(
                std::fs::read(path.join("sentinel")).unwrap(),
                b"replacement",
                "deleted {kind} replacement",
            );
            assert_eq!(std::fs::read(work.path().join("outside")).unwrap(), b"keep");
            assert_eq!(state.snapshot().removed_entries, 0);
        }

        let work = fixture();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let relative = Path::new("absent-parent/target");
        let expected = capture(&anchor, relative).unwrap();
        std::fs::create_dir(work.path().join("absent-parent")).unwrap();
        std::fs::write(work.path().join(relative), b"new").unwrap();
        assert!(
            remove_expected(&anchor, relative, &WorkerState::default(), Some(expected)).is_err()
        );
        assert_eq!(std::fs::read(work.path().join(relative)).unwrap(), b"new");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_parent_identity_survives_child_removal_and_absence_is_benign() {
        let work = fixture();
        std::fs::create_dir_all(work.path().join("parent/child")).unwrap();
        std::fs::write(work.path().join("parent/child/leaf"), b"x").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let child = capture(&anchor, Path::new("parent/child")).unwrap();
        let parent = capture(&anchor, Path::new("parent")).unwrap();
        for (relative, target) in [
            ("parent/child", child),
            ("parent", parent),
            ("parent/child", child),
            ("parent", parent),
        ] {
            remove_expected(
                &anchor,
                Path::new(relative),
                &WorkerState::default(),
                Some(target),
            )
            .unwrap()
            .outcome
            .unwrap();
        }
        assert!(!work.path().join("parent").exists());
        assert!(work.path().is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_directory_identity_is_rejected() {
        let work = fixture();
        std::fs::create_dir(work.path().join("target")).unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let captured =
            rustix::fs::statat(&parent, "target", rustix::fs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::rename(work.path().join("target"), work.path().join("original")).unwrap();
        std::fs::create_dir(work.path().join("target")).unwrap();
        std::fs::write(work.path().join("target/sentinel"), b"keep").unwrap();
        let result = open_checked_directory(
            &parent,
            std::ffi::OsStr::new("target"),
            Some(&captured),
            0,
            &WorkerState::default(),
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(work.path().join("target/sentinel")).unwrap(),
            b"keep"
        );
        assert!(work.path().join("original").is_dir());
    }
}
