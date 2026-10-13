//! Confirmed deletion runs outside the UI and remains rooted at an open directory.

use crate::backend::RootAnchor;
use crate::types::Sizes;
use crossbeam_channel::{Receiver, bounded};
#[cfg(unix)]
use rustix::fd::{AsFd, BorrowedFd};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

/// Identity captured before confirming or queueing an entry. This stores no
/// descriptors, so large batches do not exhaust the process's open-file limit.
#[derive(Clone, Copy, Debug)]
pub struct ConfirmedTarget {
    #[cfg(unix)]
    parent: Option<FileIdentity>,
    #[cfg(unix)]
    target: Option<FileIdentity>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    kind: rustix::fs::FileType,
}

#[cfg(unix)]
impl FileIdentity {
    #[allow(clippy::unnecessary_cast)] // Unix Stat widths differ by target.
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
    #[cfg(unix)]
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
    #[cfg(not(unix))]
    {
        let _ = (anchor, relative);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "anchored deletion requires Unix descriptor operations",
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

// Progress has exactly one writer: the deletion worker created with this state.
// UI handles only read snapshots or set cancellation; capture/restoration helpers
// use independent states. Keep progress writes on that one worker: increments
// deliberately avoid atomic read-modify-write instructions.
#[derive(Default)]
struct WorkerState {
    cancel: AtomicBool,
    finished: AtomicBool,
    examined_entries: AtomicU64,
    removed_entries: AtomicU64,
    removed_directories: AtomicU64,
    #[cfg(test)]
    cancel_after: AtomicU64,
    #[cfg(all(test, target_os = "linux"))]
    portable_enumeration: bool,
    #[cfg(test)]
    failed_names: std::sync::Mutex<std::collections::HashSet<std::ffi::OsString>>,
    #[cfg(test)]
    deny_next_unlink: AtomicBool,
    #[cfg(test)]
    fail_parent_restore: AtomicBool,
    #[cfg(all(test, unix))]
    unlink_hook: std::sync::Mutex<Option<TestUnlinkHook>>,
    #[cfg(all(test, unix))]
    read_hook: std::sync::Mutex<Option<TestReadHook>>,
}

#[cfg(all(test, unix))]
type TestUnlinkHook = Box<dyn FnMut(&std::ffi::OsStr, bool) -> io::Result<()> + Send>;
#[cfg(all(test, unix))]
type TestReadHook = Box<dyn FnMut(&std::ffi::OsStr) -> io::Result<()> + Send>;

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

    #[cfg(unix)]
    fn examined(&self) {
        self.examined_entries.store(
            self.examined_entries
                .load(Ordering::Relaxed)
                .wrapping_add(1),
            Ordering::Relaxed,
        );
    }

    #[cfg(unix)]
    fn removed(&self, directory: bool) {
        // Publish prior examination first, then each directory's entry removal.
        // snapshot() acquires these counters in the opposite order.
        self.removed_entries.store(
            self.removed_entries.load(Ordering::Relaxed).wrapping_add(1),
            Ordering::Release,
        );
        if directory {
            self.removed_directories.store(
                self.removed_directories
                    .load(Ordering::Relaxed)
                    .wrapping_add(1),
                Ordering::Release,
            );
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

#[derive(Clone, Debug)]
pub struct DeleteError {
    pub path: PathBuf,
    pub message: String,
}

pub struct DeleteResult {
    pub relative: PathBuf,
    /// The requested target is removed or already absent, even if cleanup failed.
    pub target_removed: bool,
    /// Interrupted means cancellation, possibly after the progress removals.
    pub outcome: io::Result<()>,
    /// Every failed entry, collected only after the requested target finishes.
    pub errors: Vec<DeleteError>,
    pub progress: DeleteProgress,
    pub parent_metadata: Option<ParentMetadata>,
    /// Parent metadata failure does not change a successful deletion outcome.
    pub metadata_error: Option<String>,
}

struct Removal {
    outcome: io::Result<()>,
    target_removed: bool,
    errors: Vec<DeleteError>,
    parent_metadata: Option<ParentMetadata>,
    metadata_error: Option<String>,
}

impl Removal {
    fn failed(error: io::Error) -> Self {
        Self {
            outcome: Err(error),
            target_removed: false,
            errors: Vec::new(),
            parent_metadata: None,
            metadata_error: None,
        }
    }

    #[cfg(unix)]
    fn already_absent() -> Self {
        Self {
            outcome: Ok(()),
            target_removed: true,
            errors: Vec::new(),
            parent_metadata: None,
            metadata_error: None,
        }
    }
}

pub struct DeleteHandle {
    pub receiver: Receiver<DeleteResult>,
    state: Arc<WorkerState>,
}

impl DeleteHandle {
    pub fn is_finished(&self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }

    /// Cooperative cancellation never waits for or joins the worker.
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Relaxed);
    }

    pub fn progress(&self) -> DeleteProgress {
        self.state.snapshot()
    }
}

use crate::completion::Wake as CompletionWake;

struct FinishedGuard {
    state: Arc<WorkerState>,
    completion: CompletionWake,
    sender: Option<crossbeam_channel::Sender<DeleteResult>>,
}

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        // On unwind, the receiver must already be disconnected when it wakes.
        // Otherwise it could observe Empty and consume the only notification.
        self.sender.take();
        self.state.finished.store(true, Ordering::Release);
        self.completion.signal();
    }
}

/// Call only after user confirmation. Dropping the handle never joins the worker.
/// The caller keeps at most one operation active and applies its result to the model.
pub fn spawn(anchor: Arc<RootAnchor>, relative: PathBuf) -> DeleteHandle {
    spawn_inner(anchor, relative, None, CompletionWake::default())
}

/// Start an operation authorized by an earlier confirmation snapshot. Replacement
/// targets or parents fail without deleting the replacement object.
pub fn spawn_confirmed(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: ConfirmedTarget,
) -> DeleteHandle {
    spawn_inner(anchor, relative, Some(target), CompletionWake::default())
}

/// Wake input polling after the result and completion state are published.
#[cfg(unix)]
pub(crate) fn spawn_with_completion(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: Option<ConfirmedTarget>,
    completion: CompletionWake,
) -> DeleteHandle {
    spawn_inner(anchor, relative, target, completion)
}

fn spawn_inner(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: Option<ConfirmedTarget>,
    completion: CompletionWake,
) -> DeleteHandle {
    let (sender, receiver) = bounded(1);
    let worker_completion = completion.clone();
    let state = Arc::new(WorkerState::default());
    let worker_state = Arc::clone(&state);
    let failure_sender = sender.clone();
    let failure_relative = relative.clone();
    let failure_path = std::path::absolute(anchor.root_path())
        .unwrap_or_else(|_| anchor.root_path().to_path_buf())
        .join(&relative);
    let worker_path = failure_path.clone();
    let result = thread::Builder::new()
        .name("ldu-delete".into())
        .spawn(move || {
            let finished_guard = FinishedGuard {
                state: worker_state,
                completion: worker_completion,
                sender: Some(sender),
            };
            let mut removal = remove_expected(&anchor, &relative, &finished_guard.state, target)
                .unwrap_or_else(Removal::failed);
            if let Err(error) = &removal.outcome
                && error.kind() != io::ErrorKind::Interrupted
                && removal.errors.is_empty()
            {
                removal.errors.push(DeleteError {
                    path: worker_path,
                    message: error.to_string(),
                });
            }
            let _ = finished_guard
                .sender
                .as_ref()
                .unwrap()
                .try_send(DeleteResult {
                    relative,
                    outcome: removal.outcome,
                    target_removed: removal.target_removed,
                    errors: removal.errors,
                    progress: finished_guard.state.snapshot(),
                    parent_metadata: removal.parent_metadata,
                    metadata_error: removal.metadata_error,
                });
        });
    if let Err(error) = result {
        let _ = failure_sender.try_send(DeleteResult {
            relative: failure_relative,
            target_removed: false,
            outcome: Err(io::Error::new(
                error.kind(),
                format!("could not start deletion worker: {error}"),
            )),
            errors: vec![DeleteError {
                path: failure_path,
                message: format!("could not start deletion worker: {error}"),
            }],
            progress: state.snapshot(),
            parent_metadata: None,
            metadata_error: None,
        });
        state.finished.store(true, Ordering::Release);
        completion.signal();
    }
    DeleteHandle { receiver, state }
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
    #[cfg(unix)]
    {
        remove_unix(anchor, &relative, state, expected)
    }
    #[cfg(not(unix))]
    {
        let _ = (anchor, relative, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "anchored deletion requires Unix descriptor operations",
        ))
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Unix targets.
fn remove_unix(
    anchor: &RootAnchor,
    relative: &Path,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
) -> io::Result<Removal> {
    let parent = match anchor.open_parent_cancellable(relative, &state.cancel) {
        Ok(parent) => parent,
        // Another queued deletion may have removed this target's parent.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Removal::already_absent());
        }
        Err(error) => return Err(error),
    };
    state.check_cancel()?;
    let held = retry_operation(state, || rustix::fs::fstat(&parent))?;
    if expected.is_some_and(|expected| expected.parent != Some(FileIdentity::from_stat(&held))) {
        return Err(changed_target());
    }
    let basename = relative.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "deletion target has no basename",
        )
    })?;
    let mut errors = Vec::new();
    let mut target_removed = false;
    let mut path = std::path::absolute(anchor.root_path())?.join(relative);
    let mut outcome = remove_from_parent(
        parent.as_fd(),
        basename,
        state,
        expected,
        &mut path,
        &mut errors,
        &mut target_removed,
        || {
            anchor.parent_ancestors_cancellable(
                relative,
                (held.st_dev as u64, held.st_ino as u64),
                &state.cancel,
            )
        },
    );
    if let Err(error) = &outcome {
        if error.kind() != io::ErrorKind::Interrupted {
            errors.push(DeleteError {
                path,
                message: error.to_string(),
            });
        }
    } else if !errors.is_empty() {
        outcome = Err(io::Error::other(format!(
            "{} entries could not be deleted",
            errors.len()
        )));
    }
    // Cancellation must not start more filesystem work just to refresh totals.
    // The caller can reconcile surviving descendants with a scoped refresh.
    if state.cancel.load(Ordering::Relaxed) {
        return Ok(Removal {
            outcome,
            target_removed,
            errors,
            parent_metadata: None,
            metadata_error: None,
        });
    }
    let (parent_metadata, metadata_error) =
        match retry_operation(state, || rustix::fs::fstat(&parent)) {
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
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (None, None),
            Err(error) => (
                None,
                Some(format!("could not refresh parent metadata: {error}")),
            ),
        };
    Ok(Removal {
        outcome,
        target_removed,
        errors,
        parent_metadata,
        metadata_error,
    })
}

#[cfg(unix)]
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

#[cfg(unix)]
fn changed_target() -> io::Error {
    io::Error::other("deletion target changed before removal; refresh and confirm again")
}

#[cfg(unix)]
fn checked_current(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    captured: FileIdentity,
    state: &WorkerState,
) -> io::Result<bool> {
    use rustix::fs::{AtFlags, statat};
    match retry_operation(state, || statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(current) if captured == FileIdentity::from_stat(&current) => Ok(true),
        Ok(_) => Err(changed_target()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Keep one descriptor per active depth. Constructing or reopening a buffered
/// stream needs one temporary extra descriptor; failure preserves the old one.
#[cfg(unix)]
enum DirectoryHandle {
    Raw(rustix::fd::OwnedFd),
    #[cfg(any(test, not(target_os = "linux")))]
    Buffered(rustix::fs::Dir),
}

#[cfg(unix)]
impl DirectoryHandle {
    fn fd(&self) -> io::Result<BorrowedFd<'_>> {
        match self {
            Self::Raw(fd) => Ok(fd.as_fd()),
            #[cfg(any(test, not(target_os = "linux")))]
            Self::Buffered(entries) => entries.fd().map_err(io::Error::from),
        }
    }

    #[cfg(target_os = "linux")]
    fn raw_fd(&self) -> io::Result<&rustix::fd::OwnedFd> {
        match self {
            Self::Raw(fd) => Ok(fd),
            #[cfg(test)]
            Self::Buffered(_) => Err(io::Error::other("buffered directory has no raw reader")),
        }
    }

    #[cfg(any(test, not(target_os = "linux")))]
    fn prepare_buffered(&mut self, state: &WorkerState, reopen: bool) -> io::Result<()> {
        if !reopen && matches!(self, Self::Buffered(_)) {
            return Ok(());
        }
        let fd = self.fd()?;
        let entries = retry_operation(state, || rustix::fs::Dir::read_from(fd))?;
        // Do not drop the old descriptor until its replacement is usable.
        *self = Self::Buffered(entries);
        Ok(())
    }

    #[cfg(any(test, not(target_os = "linux")))]
    fn entries_mut(&mut self) -> io::Result<&mut rustix::fs::Dir> {
        match self {
            Self::Buffered(entries) => Ok(entries),
            Self::Raw(_) => Err(io::Error::other("directory stream is not initialized")),
        }
    }
}

#[cfg(unix)]
#[allow(clippy::box_collection)]
type RetrySkips = Option<Box<std::collections::HashSet<std::ffi::OsString>>>;

#[cfg(unix)]
struct DirectoryFrame {
    handle: DirectoryHandle,
    #[cfg(any(test, not(target_os = "linux")))]
    reopen_stream: bool,
    name: std::ffi::OsString,
    captured: FileIdentity,
    pending: Vec<DirectoryName>,
    exhausted: bool,
    retried_removal: bool,
    retried_read: bool,
    read_error: Option<io::Error>,
    errors_at_entry: usize,
    // Keep successful traversal frames compact; allocate the set only after failure.
    retry_skip: RetrySkips,
}

#[cfg(unix)]
struct DirectoryName {
    name: std::ffi::OsString,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn descriptor_path(fd: BorrowedFd<'_>) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Permission repair always names a held inode; symlinks are rejected.
#[cfg(unix)]
fn chmod_held(fd: BorrowedFd<'_>, mode: rustix::fs::Mode, state: &WorkerState) -> io::Result<()> {
    let held = retry_operation(state, || rustix::fs::fstat(fd))?;
    if rustix::fs::FileType::from_raw_mode(held.st_mode) == rustix::fs::FileType::Symlink {
        return Err(io::Error::other(
            "cannot change permissions through a symlink",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // Linux O_PATH cannot use fchmod; proc resolves the retained inode.
        let path = descriptor_path(fd);
        let visible = retry_operation(state, || rustix::fs::stat(&path))?;
        if !identity_matches(&held, &visible) {
            return Err(changed_target());
        }
        retry_operation(state, || rustix::fs::chmod(&path, mode))
    }
    #[cfg(target_os = "freebsd")]
    {
        // FreeBSD fchmod rejects O_PATH, but AT_EMPTY_PATH explicitly operates
        // on that descriptor and does not require the directory to be readable.
        retry_operation(state, || {
            rustix::fs::chmodat(fd, "", mode, rustix::fs::AtFlags::EMPTY_PATH)
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        retry_operation(state, || rustix::fs::fchmod(fd, mode))
    }
}

#[cfg(unix)]
fn chown_held(fd: BorrowedFd<'_>, state: &WorkerState) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        retry_operation(state, || {
            rustix::fs::chownat(
                fd,
                "",
                Some(rustix::process::geteuid()),
                None,
                rustix::fs::AtFlags::EMPTY_PATH,
            )
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        retry_operation(state, || {
            rustix::fs::fchown(fd, Some(rustix::process::geteuid()), None)
        })
    }
}

#[cfg(unix)]
fn ensure_parent_mode_recoverable(
    original: rustix::fs::Mode,
    group: u32,
    effective_group: u32,
    supplementary_member: impl FnOnce() -> io::Result<bool>,
) -> io::Result<()> {
    if original.contains(rustix::fs::Mode::SGID)
        && group != effective_group
        && !supplementary_member()?
    {
        // chmod can succeed while silently clearing SGID. Conservatively refuse
        // external-parent changes unless group membership guarantees preservation.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot preserve the containing directory's setgid permission",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_held_mode(
    fd: BorrowedFd<'_>,
    expected: rustix::fs::Mode,
    state: &WorkerState,
) -> io::Result<()> {
    let stat = retry_operation(state, || rustix::fs::fstat(fd))?;
    let actual = rustix::fs::Mode::from_raw_mode(stat.st_mode);
    if actual != expected {
        return Err(io::Error::other(format!(
            "permission restoration did not preserve mode {:04o}; found {:04o}",
            expected.bits(),
            actual.bits(),
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn repair_directory_permissions(
    fd: BorrowedFd<'_>,
    state: &WorkerState,
    allow_ownership: bool,
) -> io::Result<Option<rustix::fs::Mode>> {
    use rustix::fs::{FileType, Mode, fstat};
    let stat = retry_operation(state, || fstat(fd))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(changed_target());
    }
    let original = Mode::from_raw_mode(stat.st_mode);
    let repaired = original | Mode::RWXU;
    if !allow_ownership {
        // Avoid both a no-op chmod and a later restoration of an unchanged mode.
        if repaired == original {
            return Ok(None);
        }
        ensure_parent_mode_recoverable(
            original,
            stat.st_gid,
            rustix::process::getegid().as_raw(),
            || {
                retry_operation(state, rustix::process::getgroups)
                    .map(|groups| groups.iter().any(|group| group.as_raw() == stat.st_gid))
            },
        )?;
    }
    if let Err(mode_error) = chmod_held(fd, repaired, state) {
        if !allow_ownership || mode_error.kind() != io::ErrorKind::PermissionDenied {
            return Err(mode_error);
        }
        // This uses only the process's existing credentials; no elevation or
        // recursive ownership changes are attempted.
        chown_held(fd, state).map_err(|owner_error| {
            io::Error::new(
                owner_error.kind(),
                format!(
                    "permission repair failed: {mode_error}; ownership repair failed: {owner_error}"
                ),
            )
        })?;
        chmod_held(fd, repaired, state)?;
    }
    Ok(Some(original))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // ino_t widths differ by target.
fn checked_path_handle(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    expected: Option<FileIdentity>,
    inode: u64,
    state: &WorkerState,
) -> io::Result<(rustix::fd::OwnedFd, rustix::fs::Stat)> {
    use rustix::fs::{Mode, OFlags, fstat, openat};
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    // Darwin's process policy may still require read access for O_EVTONLY.
    // If it rejects this handle, leave the entry untouched and report it.
    #[cfg(target_os = "macos")]
    let flags = OFlags::from_bits_retain(libc::O_EVTONLY as u32)
        | OFlags::DIRECTORY
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW;
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = retry_operation(state, || openat(parent, name, flags, Mode::empty()))?;
    let captured = retry_operation(state, || fstat(&fd))?;
    if expected.is_some_and(|expected| expected != FileIdentity::from_stat(&captured))
        || (inode != 0 && inode != captured.st_ino as u64)
    {
        return Err(changed_target());
    }
    Ok((fd, captured))
}

/// Linux and FreeBSD can retain every inode using O_PATH. Elsewhere a
/// no-follow stat records identity without opening ordinary files or devices.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // ino_t widths differ by target.
fn checked_retry_target(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    expected: Option<FileIdentity>,
    inode: u64,
    state: &WorkerState,
) -> io::Result<(Option<rustix::fd::OwnedFd>, rustix::fs::Stat)> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        checked_path_handle(parent, name, expected, inode, state).map(|(fd, stat)| (Some(fd), stat))
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let stat = retry_operation(state, || {
            rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        })?;
        if expected.is_some_and(|expected| expected != FileIdentity::from_stat(&stat))
            || (inode != 0 && inode != stat.st_ino as u64)
        {
            return Err(changed_target());
        }
        Ok((None, stat))
    }
}

#[cfg(unix)]
fn repair_target_ownership(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    target: Option<&rustix::fd::OwnedFd>,
    captured: &rustix::fs::Stat,
    state: &WorkerState,
) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let _ = (parent, name, captured);
        chown_held(
            target.expect("O_PATH retry captures a held inode").as_fd(),
            state,
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
        let _ = target;
        let kind = FileType::from_raw_mode(captured.st_mode);
        if !matches!(kind, FileType::RegularFile | FileType::Directory) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot safely retain this entry for ownership repair",
            ));
        }
        // NONBLOCK prevents a concurrently substituted FIFO from blocking. The
        // post-open identity check rejects any replacement before changing it.
        #[cfg(target_os = "macos")]
        let access = OFlags::from_bits_retain(libc::O_EVTONLY as u32);
        #[cfg(not(target_os = "macos"))]
        let access = OFlags::RDONLY;
        let mut flags = access | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        if kind == FileType::Directory {
            flags |= OFlags::DIRECTORY;
        }
        let held = retry_operation(state, || openat(parent, name, flags, Mode::empty()))?;
        let current = retry_operation(state, || fstat(&held))?;
        if FileIdentity::from_stat(&current) != FileIdentity::from_stat(captured) {
            return Err(changed_target());
        }
        chown_held(held.as_fd(), state)
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // ino_t widths differ by target.
fn open_checked_directory(
    parent: BorrowedFd<'_>,
    name: std::ffi::OsString,
    expected: Option<FileIdentity>,
    inode: u64,
    state: &WorkerState,
    ancestors: &std::collections::HashSet<(u64, u64)>,
) -> io::Result<Option<DirectoryFrame>> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    if let Some(captured) = expected {
        reject_directory_ancestor(ancestors, captured)?;
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = match retry_operation(state, || openat(parent, &name, flags, Mode::empty())) {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let (held, captured) = match checked_path_handle(parent, &name, expected, inode, state)
            {
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied && expected.is_none() =>
                {
                    // An enumerated directory can be readable but not searchable.
                    // Its retained parent is inside the confirmed subtree.
                    repair_directory_permissions(parent, state, true)?;
                    checked_path_handle(parent, &name, expected, inode, state)?
                }
                result => result?,
            };
            if FileType::from_raw_mode(captured.st_mode) != FileType::Directory {
                return Err(changed_target());
            }
            reject_directory_ancestor(ancestors, FileIdentity::from_stat(&captured))?;
            if !checked_current(parent, &name, FileIdentity::from_stat(&captured), state)? {
                return Ok(None);
            }
            repair_directory_permissions(held.as_fd(), state, true)?;
            // Reopen the held directory itself, never resolve the entry again.
            retry_operation(state, || openat(&held, ".", flags, Mode::empty()))?
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
        Err(_) => match retry_operation(state, || openat(parent, &name, flags, Mode::empty())) {
            Ok(fd) => fd,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        },
    };
    let captured = retry_operation(state, || fstat(&fd))?;
    if expected.is_some_and(|expected| expected != FileIdentity::from_stat(&captured))
        || (inode != 0 && inode != captured.st_ino as u64)
    {
        return Err(changed_target());
    }
    reject_directory_ancestor(ancestors, FileIdentity::from_stat(&captured))?;
    if !checked_current(parent, &name, FileIdentity::from_stat(&captured), state)? {
        return Ok(None);
    }
    Ok(Some(DirectoryFrame {
        handle: DirectoryHandle::Raw(fd),
        #[cfg(any(test, not(target_os = "linux")))]
        reopen_stream: false,
        name,
        captured: FileIdentity::from_stat(&captured),
        pending: Vec::new(),
        exhausted: false,
        retried_removal: false,
        retried_read: false,
        read_error: None,
        errors_at_entry: 0,
        retry_skip: None,
    }))
}

#[cfg(unix)]
fn unlink_entry(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg + Copy,
    directory: bool,
    state: &WorkerState,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, unlinkat};
    #[cfg(test)]
    {
        use std::os::unix::ffi::OsStrExt;
        if state.deny_next_unlink.swap(false, Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let name = name.as_cow_c_str()?;
        if let Some(hook) = state.unlink_hook.lock().unwrap().as_mut() {
            state.check_cancel()?;
            hook(std::ffi::OsStr::from_bytes(name.to_bytes()), directory)?;
        }
        if state
            .failed_names
            .lock()
            .unwrap()
            .contains(std::ffi::OsStr::from_bytes(name.to_bytes()))
        {
            return Err(io::Error::other("injected removal failure"));
        }
    }
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

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn retry_failed_unlink(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    directory: bool,
    captured: Option<FileIdentity>,
    inode: u64,
    restore_parent: bool,
    state: &WorkerState,
    original_error: io::Error,
    target_removed: Option<&mut bool>,
) -> io::Result<()> {
    use rustix::fs::FileType;
    if original_error.kind() == io::ErrorKind::Interrupted {
        return Err(original_error);
    }
    let permission_denied = original_error.kind() == io::ErrorKind::PermissionDenied;
    let (target, captured) = match checked_retry_target(parent, name, captured, inode, state) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(target_removed) = target_removed {
                *target_removed = true;
            }
            return Ok(());
        }
        Err(error)
            if permission_denied
                && error.kind() == io::ErrorKind::PermissionDenied
                && !restore_parent =>
        {
            repair_directory_permissions(parent, state, true)?;
            checked_retry_target(parent, name, captured, inode, state)?
        }
        result => result?,
    };
    if (FileType::from_raw_mode(captured.st_mode) == FileType::Directory) != directory {
        return Err(changed_target());
    }
    if !checked_current(parent, name, FileIdentity::from_stat(&captured), state)? {
        if let Some(target_removed) = target_removed {
            *target_removed = true;
        }
        return Ok(());
    }
    let repaired = if permission_denied {
        repair_directory_permissions(parent, state, !restore_parent)
    } else {
        Ok(None)
    };
    // Sticky directories may additionally require ownership of the selected
    // inode. Recovery acts through a retained descriptor and never follows links.
    let ownership_error = if permission_denied
        && FileType::from_raw_mode(captured.st_mode) != FileType::Symlink
        && captured.st_uid != rustix::process::geteuid().as_raw()
    {
        repair_target_ownership(parent, name, target.as_ref(), &captured, state).err()
    } else {
        None
    };
    let retry = checked_current(parent, name, FileIdentity::from_stat(&captured), state).and_then(
        |exists| {
            if exists {
                unlink_entry(parent, name, directory, state)
            } else {
                Ok(())
            }
        },
    );
    // Record the unlink boundary before restoring an external parent's mode.
    // Restoration diagnostics must not resurrect a successfully removed target.
    if retry.is_ok()
        && let Some(target_removed) = target_removed
    {
        *target_removed = true;
    }
    // The target's containing directory can lie outside the requested subtree.
    // Restore its permissions even after cooperative cancellation.
    if restore_parent && let Ok(Some(original_mode)) = repaired.as_ref() {
        let restore_state = WorkerState::default();
        #[cfg(test)]
        let restoration = if state.fail_parent_restore.load(Ordering::Relaxed) {
            Err(io::Error::other("injected parent restoration failure"))
        } else {
            chmod_held(parent, *original_mode, &restore_state)
        };
        #[cfg(not(test))]
        let restoration = chmod_held(parent, *original_mode, &restore_state);
        let restoration =
            restoration.and_then(|()| verify_held_mode(parent, *original_mode, &restore_state));
        restoration.map_err(|error| {
            #[cfg(target_os = "linux")]
            let parent_name = std::fs::read_link(descriptor_path(parent))
                .unwrap_or_else(|_| descriptor_path(parent))
                .display()
                .to_string();
            #[cfg(not(target_os = "linux"))]
            let parent_name = "the containing directory";
            io::Error::new(
                error.kind(),
                format!("could not restore permissions of {}: {error}", parent_name),
            )
        })?;
    }
    match retry {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::Interrupted
                || (repaired.is_ok() && ownership_error.is_none()) =>
        {
            Err(error)
        }
        Err(error) => {
            let mut message = error.to_string();
            if let Err(repair_error) = repaired {
                message.push_str(&format!("; permission recovery failed: {repair_error}"));
            }
            if let Some(owner_error) = ownership_error {
                message.push_str(&format!("; ownership recovery failed: {owner_error}"));
            }
            Err(io::Error::new(error.kind(), message))
        }
    }
}

#[cfg(unix)]
fn record_error(
    errors: &mut Vec<DeleteError>,
    path: impl Into<PathBuf>,
    error: io::Error,
) -> io::Result<()> {
    if error.kind() == io::ErrorKind::Interrupted {
        return Err(error);
    }
    errors.push(DeleteError {
        path: path.into(),
        message: error.to_string(),
    });
    Ok(())
}

/// Keep logical ancestors above the selected target for the whole job. Only
/// active deletion frames leave the set: a removed sibling can free an inode
/// for reuse, while the selected target's parent chain remains out of scope.
#[cfg(unix)]
fn reject_directory_ancestor(
    ancestors: &std::collections::HashSet<(u64, u64)>,
    captured: FileIdentity,
) -> io::Result<()> {
    if ancestors.contains(&(captured.device, captured.inode)) {
        Err(io::Error::other(
            "directory refers to an active ancestor; contents skipped",
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn enter_directory(
    ancestors: &mut std::collections::HashSet<(u64, u64)>,
    captured: FileIdentity,
) -> io::Result<()> {
    if ancestors.insert((captured.device, captured.inode)) {
        Ok(())
    } else {
        Err(io::Error::other(
            "directory refers to an active ancestor; contents skipped",
        ))
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)] // Load directory ancestors only after the leaf fast path.
fn remove_from_parent(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
    path: &mut PathBuf,
    errors: &mut Vec<DeleteError>,
    target_removed: &mut bool,
    parent_ancestors: impl FnOnce() -> io::Result<std::collections::HashSet<(u64, u64)>>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, statat};
    let captured = match retry_operation(state, || statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
    {
        Ok(captured) => captured,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            *target_removed = true;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let captured = FileIdentity::from_stat(&captured);
    state.examined();
    if expected.is_some_and(|expected| expected.target != Some(captured)) {
        return Err(changed_target());
    }
    if !checked_current(parent, name, captured, state)? {
        *target_removed = true;
        return Ok(());
    }
    if captured.kind != FileType::Directory {
        return match unlink_entry(parent, name, false, state) {
            Ok(()) => {
                *target_removed = true;
                Ok(())
            }
            Err(error) => retry_failed_unlink(
                parent,
                name,
                false,
                Some(captured),
                0,
                true,
                state,
                error,
                Some(target_removed),
            ),
        };
    }
    let mut ancestors = parent_ancestors()?;
    let Some(root) = open_checked_directory(
        parent,
        name.to_owned(),
        Some(captured),
        0,
        state,
        &ancestors,
    )?
    else {
        *target_removed = true;
        return Ok(());
    };
    enter_directory(&mut ancestors, root.captured)?;
    let mut stack = vec![root];
    #[cfg(target_os = "linux")]
    let mut buffer = {
        let mut buffer = Vec::<u8>::new();
        buffer
            .try_reserve_exact(64 * 1024)
            .map_err(io::Error::other)?;
        buffer
    };
    #[cfg(target_os = "linux")]
    let mut leaves = LeafUnlinks::default();
    while !stack.is_empty() {
        state.check_cancel()?;
        if let Some(next) = stack.last_mut().unwrap().pending.pop() {
            let frame = stack.last().unwrap();
            if frame
                .retry_skip
                .as_ref()
                .is_some_and(|skipped| skipped.contains(next.name.as_os_str()))
            {
                continue;
            }
            path.push(&next.name);
            match open_checked_directory(
                frame.handle.fd()?,
                next.name,
                None,
                next.inode,
                state,
                &ancestors,
            ) {
                Ok(Some(mut child)) => {
                    if let Err(error) = enter_directory(&mut ancestors, child.captured) {
                        record_error(errors, path.as_path(), error)?;
                        remember_failed_child(
                            &mut stack.last_mut().unwrap().retry_skip,
                            path.file_name().unwrap(),
                        );
                    } else {
                        child.errors_at_entry = errors.len();
                        stack.push(child);
                        continue;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    record_error(errors, path.as_path(), error)?;
                    remember_failed_child(
                        &mut stack.last_mut().unwrap().retry_skip,
                        path.file_name().unwrap(),
                    );
                }
            }
            path.pop();
            continue;
        }
        let frame = stack.last_mut().unwrap();
        if !frame.exhausted {
            #[cfg(all(target_os = "linux", not(test)))]
            let batch = read_directory_batch(frame, &mut buffer, &mut leaves, state, path, errors);
            #[cfg(all(target_os = "linux", test))]
            let batch = if state.portable_enumeration {
                read_portable_directory_batch(frame, state, path, errors)
            } else {
                read_directory_batch(frame, &mut buffer, &mut leaves, state, path, errors)
            };
            #[cfg(not(target_os = "linux"))]
            let batch = read_portable_directory_batch(frame, state, path, errors);
            match batch {
                Ok(exhausted) => frame.exhausted = exhausted,
                Err(error) => {
                    if error.kind() == io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                    if !frame.retried_read {
                        frame.retried_read = true;
                        #[cfg(any(test, not(target_os = "linux")))]
                        {
                            // libc Dir stops after an error. Reopen the held
                            // directory once; the next batch excludes failures
                            // after all pending children have finished.
                            frame.reopen_stream = true;
                        }
                        if error.kind() == io::ErrorKind::PermissionDenied
                            && let Err(error) =
                                repair_directory_permissions(frame.handle.fd()?, state, true)
                        {
                            frame.exhausted = true;
                            frame.read_error = Some(error);
                        }
                        continue;
                    }
                    frame.exhausted = true;
                    frame.read_error = Some(error);
                }
            }
            continue;
        }
        let mut finished = stack.pop().unwrap();
        ancestors.remove(&(finished.captured.device, finished.captured.inode));
        let is_target = stack.is_empty();
        let containing_parent = match stack.last() {
            Some(frame) => frame.handle.fd()?,
            None => parent,
        };
        let mut directory_removed = false;
        let result = checked_current(containing_parent, &finished.name, finished.captured, state)
            .and_then(|exists| {
                if !exists {
                    directory_removed = true;
                    return Ok(());
                }
                match unlink_entry(containing_parent, &finished.name, true, state) {
                    Ok(()) => {
                        directory_removed = true;
                        Ok(())
                    }
                    Err(error)
                        if finished.retried_removal
                            || error.raw_os_error()
                                == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) =>
                    {
                        Err(error)
                    }
                    Err(error) => {
                        finished.retried_removal = true;
                        retry_failed_unlink(
                            containing_parent,
                            &finished.name,
                            true,
                            Some(finished.captured),
                            0,
                            is_target,
                            state,
                            error,
                            Some(&mut directory_removed),
                        )
                    }
                }
            });
        if directory_removed {
            // A successful rmdir (or confirmed absence) proves that earlier
            // descendant failures no longer remain inside this directory.
            // Parent permission restoration errors are recorded below.
            errors.truncate(finished.errors_at_entry);
            if is_target {
                *target_removed = true;
            }
        }
        if let Err(mut error) = result {
            if !finished.retried_removal
                && finished.read_error.is_none()
                && error.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error())
            {
                match prepare_directory_retry(&mut finished, containing_parent, state) {
                    Ok(true) => {
                        ancestors.insert((finished.captured.device, finished.captured.inode));
                        stack.push(finished);
                        continue;
                    }
                    Ok(false) => {
                        errors.truncate(finished.errors_at_entry);
                        if is_target {
                            *target_removed = true;
                        }
                        if !stack.is_empty() {
                            path.pop();
                        }
                        continue;
                    }
                    Err(retry_error) => error = retry_error,
                }
            }
            // A read failure is irrelevant if removal proved the directory empty.
            // Keep any external-parent permission restoration error separately.
            if !(is_target && *target_removed)
                && let Some(read_error) = finished.read_error.take()
            {
                record_error(errors, path.as_path(), read_error)?;
            }
            // A retained descendant already explains why its ancestors remain.
            if !(errors.len() > finished.errors_at_entry
                && error.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()))
            {
                record_error(errors, path.as_path(), error)?;
            }
        }
        if let Some(frame) = stack.last_mut() {
            // Retained children have exhausted their own retry budget. Their
            // diagnostics stay in the report, but only this direct name is
            // needed if an ancestor's nonempty retry enumerates them again.
            if !directory_removed {
                remember_failed_child(&mut frame.retry_skip, &finished.name);
            }
            path.pop();
        }
    }
    Ok(())
}

/// Revisit only children that have not exhausted their own attempts. Keeping
/// failed subtrees out of ancestor rescans bounds both retries and diagnostics.
#[cfg(unix)]
fn prepare_directory_retry(
    frame: &mut DirectoryFrame,
    parent: BorrowedFd<'_>,
    state: &WorkerState,
) -> io::Result<bool> {
    if !checked_current(parent, &frame.name, frame.captured, state)? {
        return Ok(false);
    }
    match &mut frame.handle {
        DirectoryHandle::Raw(fd) => {
            #[cfg(target_os = "linux")]
            retry_operation(state, || {
                rustix::fs::seek(fd.as_fd(), rustix::fs::SeekFrom::Start(0))
            })?;
            #[cfg(not(target_os = "linux"))]
            let _ = fd;
        }
        #[cfg(any(test, not(target_os = "linux")))]
        DirectoryHandle::Buffered(entries) => entries.rewind(),
    }
    frame.retried_removal = true;
    frame.exhausted = false;
    Ok(true)
}

/// Remember failures where they occur instead of rescanning every descendant
/// diagnostic at each ancestor. Successful traversal keeps this set absent.
#[cfg(unix)]
fn remember_failed_child(skips: &mut RetrySkips, name: &std::ffi::OsStr) {
    let skipped = skips.get_or_insert_with(Box::default);
    if !skipped.contains(name) {
        skipped.insert(name.to_owned());
    }
}

#[cfg(target_os = "linux")]
const LEAF_UNLINK_WINDOW: usize = crate::uring::BATCH_LIMIT;

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LeafUnlinks {
    bytes: Vec<u8>,
    entries: Vec<LeafName>,
    enabled: bool,
}

#[cfg(target_os = "linux")]
struct LeafName {
    end: usize,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl LeafUnlinks {
    fn push(&mut self, name: &std::ffi::OsStr, inode: u64) {
        use std::os::unix::ffi::OsStrExt;
        self.bytes.extend_from_slice(name.as_bytes());
        self.entries.push(LeafName {
            end: self.bytes.len(),
            inode,
        });
    }

    fn name(&self, index: usize) -> &std::ffi::OsStr {
        use std::os::unix::ffi::OsStrExt;
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.entries[previous].end);
        std::ffi::OsStr::from_bytes(&self.bytes[start..self.entries[index].end])
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.entries.clear();
    }
}

#[cfg(target_os = "linux")]
fn leaf_unlinks_available(state: &WorkerState) -> bool {
    // Deterministic syscall hooks exercise the unchanged blocking path. Batch
    // result handling has separate tests that do not depend on host io_uring.
    #[cfg(test)]
    if state.cancel_after.load(Ordering::Relaxed) != 0
        || state.deny_next_unlink.load(Ordering::Relaxed)
        || !state.failed_names.lock().unwrap().is_empty()
        || state.unlink_hook.lock().unwrap().is_some()
    {
        return false;
    }
    let _ = state;
    crate::uring::unlink_available()
}

#[cfg(target_os = "linux")]
fn apply_leaf_completions(
    parent: BorrowedFd<'_>,
    leaves: &LeafUnlinks,
    state: &WorkerState,
    path: &Path,
    errors: &mut Vec<DeleteError>,
    skipped: &mut RetrySkips,
    outcome: crate::uring::BatchOutcome<()>,
) -> io::Result<()> {
    use crate::uring::{BatchOutcome, EntryOutcome};
    let (entries, driver_error) = match outcome {
        BatchOutcome::Unsupported => (None, None),
        BatchOutcome::Complete(entries) => (Some(entries), None),
        BatchOutcome::Failed { entries, error } => (Some(entries), Some(error)),
    };
    // Cancellation can arrive after several accepted unlinks complete. Publish
    // every known success before stopping retries or unsubmitted work.
    if let Some(entries) = &entries {
        debug_assert_eq!(entries.len(), leaves.entries.len());
        for result in entries {
            if matches!(result, EntryOutcome::Completed(Ok(()))) {
                state.removed(false);
            }
        }
    }
    state.check_cancel()?;
    let mut entries = entries.map(Vec::into_iter);
    for (index, leaf) in leaves.entries.iter().enumerate() {
        let name = leaves.name(index);
        let status = entries
            .as_mut()
            .map_or(EntryOutcome::NotSubmitted, |entries| {
                entries.next().unwrap()
            });
        let initial = match status {
            EntryOutcome::Completed(Ok(())) => continue,
            EntryOutcome::Completed(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            EntryOutcome::Completed(Err(error)) if error.kind() != io::ErrorKind::Interrupted => {
                Err(error)
            }
            EntryOutcome::Completed(Err(_)) | EntryOutcome::NotSubmitted => {
                // A completed EINTR did not consume the ordinary failure retry.
                // The existing wrapper retries signals and observes cancellation.
                unlink_entry(parent, name, false, state)
            }
            EntryOutcome::Unknown => {
                // Failed batches return only after their driver has closed.
                // Absence is safe to inspect, but never replay an uncertain
                // unlink: this name may now refer to a newly created entry.
                match retry_operation(state, || {
                    rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                }) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                    _ => {}
                }
                let message = match &driver_error {
                    Some(error) => format!("deletion completion could not be confirmed: {error}"),
                    None => "deletion completion could not be confirmed".to_owned(),
                };
                record_error(errors, path.join(name), io::Error::other(message))?;
                remember_failed_child(skipped, name);
                continue;
            }
        };
        if let Err(error) = initial
            && let Err(error) = retry_failed_unlink(
                parent, name, false, None, leaf.inode, false, state, error, None,
            )
        {
            record_error(errors, path.join(name), error)?;
            remember_failed_child(skipped, name);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn flush_leaf_unlinks(
    parent: &rustix::fd::OwnedFd,
    leaves: &mut LeafUnlinks,
    state: &WorkerState,
    path: &Path,
    errors: &mut Vec<DeleteError>,
    skipped: &mut RetrySkips,
) -> io::Result<()> {
    if leaves.entries.is_empty() {
        return Ok(());
    }
    // RawDir may refill its buffer on next(), so retain names in one reusable
    // packed buffer. Only the submitted operation needs a per-name CString.
    let mut names = [std::ffi::OsStr::new(""); LEAF_UNLINK_WINDOW];
    for (index, name) in names.iter_mut().take(leaves.entries.len()).enumerate() {
        *name = leaves.name(index);
    }
    let outcome = crate::uring::unlink_batch(parent, &names[..leaves.entries.len()], &state.cancel);
    if !matches!(&outcome, crate::uring::BatchOutcome::Complete(_)) {
        leaves.enabled = false;
    }
    let result = apply_leaf_completions(
        parent.as_fd(),
        leaves,
        state,
        path,
        errors,
        skipped,
        outcome,
    );
    leaves.clear();
    result
}

/// Consume one whole getdents buffer before suspending its descriptor. Known
/// leaves use bounded io_uring batches when available; blocking deletion keeps
/// borrowed names and allocates full paths only for final errors.
#[cfg(target_os = "linux")]
fn read_directory_batch(
    frame: &mut DirectoryFrame,
    buffer: &mut Vec<u8>,
    leaves: &mut LeafUnlinks,
    state: &WorkerState,
    path: &Path,
    errors: &mut Vec<DeleteError>,
) -> io::Result<bool> {
    use rustix::fs::{AtFlags, FileType, RawDir, statat};
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;
    // The caller drains each batch before reading again. Retain its allocations
    // for wide directory trees instead of rebuilding them for every getdents.
    debug_assert!(frame.pending.is_empty());
    debug_assert!(leaves.entries.is_empty());
    leaves.enabled = leaf_unlinks_available(state);
    #[cfg(test)]
    if let Some(hook) = state.read_hook.lock().unwrap().as_mut() {
        state.check_cancel()?;
        hook(&frame.name)?;
    }
    let directory = frame.handle.raw_fd()?;
    let fd = directory.as_fd();
    loop {
        let capacity = buffer.capacity();
        {
            let mut entries = RawDir::new(fd, buffer.spare_capacity_mut());
            loop {
                state.check_cancel()?;
                let Some(entry) = entries.next() else {
                    flush_leaf_unlinks(
                        directory,
                        leaves,
                        state,
                        path,
                        errors,
                        &mut frame.retry_skip,
                    )?;
                    return Ok(true);
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(Errno::INTR) => continue,
                    Err(Errno::INVAL) if capacity < 1024 * 1024 => {
                        flush_leaf_unlinks(
                            directory,
                            leaves,
                            state,
                            path,
                            errors,
                            &mut frame.retry_skip,
                        )?;
                        break;
                    }
                    Err(error) => {
                        flush_leaf_unlinks(
                            directory,
                            leaves,
                            state,
                            path,
                            errors,
                            &mut frame.retry_skip,
                        )?;
                        return Err(error.into());
                    }
                };
                let bytes = entry.file_name().to_bytes();
                if bytes != b"." && bytes != b".." {
                    let name = std::ffi::OsStr::from_bytes(bytes);
                    if frame
                        .retry_skip
                        .as_ref()
                        .is_some_and(|skipped| skipped.contains(name))
                    {
                        if entries.is_buffer_empty() {
                            flush_leaf_unlinks(
                                directory,
                                leaves,
                                state,
                                path,
                                errors,
                                &mut frame.retry_skip,
                            )?;
                            return Ok(false);
                        }
                        continue;
                    }
                    state.examined();
                    let kind = entry.file_type();
                    let mut is_directory = kind == FileType::Directory;
                    if leaves.enabled && !is_directory && kind != FileType::Unknown {
                        leaves.push(name, entry.ino());
                        if leaves.entries.len() == LEAF_UNLINK_WINDOW {
                            flush_leaf_unlinks(
                                directory,
                                leaves,
                                state,
                                path,
                                errors,
                                &mut frame.retry_skip,
                            )?;
                        }
                    } else if !is_directory {
                        // Unknown d_type classification and its permission repair
                        // remain serial, after previously admitted leaf work.
                        flush_leaf_unlinks(
                            directory,
                            leaves,
                            state,
                            path,
                            errors,
                            &mut frame.retry_skip,
                        )?;
                        if let Err(error) = unlink_entry(fd, entry.file_name(), false, state) {
                            let mut captured = None;
                            if kind == FileType::Unknown
                                && (error.kind() == io::ErrorKind::PermissionDenied
                                    || error.raw_os_error() == Some(Errno::ISDIR.raw_os_error()))
                            {
                                let inspected = retry_operation(state, || {
                                    statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
                                })
                                .or_else(|error| {
                                    if error.kind() != io::ErrorKind::PermissionDenied {
                                        return Err(error);
                                    }
                                    repair_directory_permissions(fd, state, true)?;
                                    retry_operation(state, || {
                                        statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
                                    })
                                });
                                match inspected {
                                    Ok(stat) => {
                                        is_directory = FileType::from_raw_mode(stat.st_mode)
                                            == FileType::Directory;
                                        captured = Some(FileIdentity::from_stat(&stat));
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                        continue;
                                    }
                                    Err(error) => {
                                        record_error(errors, path.join(name), error)?;
                                        remember_failed_child(&mut frame.retry_skip, name);
                                        continue;
                                    }
                                }
                            }
                            if !is_directory
                                && let Err(error) = retry_failed_unlink(
                                    fd,
                                    name,
                                    false,
                                    captured,
                                    entry.ino(),
                                    false,
                                    state,
                                    error,
                                    None,
                                )
                            {
                                record_error(errors, path.join(name), error)?;
                                remember_failed_child(&mut frame.retry_skip, name);
                            }
                        }
                    }
                    if is_directory {
                        frame.pending.push(DirectoryName {
                            name: name.to_owned(),
                            inode: entry.ino(),
                        });
                    }
                }
                if entries.is_buffer_empty() {
                    flush_leaf_unlinks(
                        directory,
                        leaves,
                        state,
                        path,
                        errors,
                        &mut frame.retry_skip,
                    )?;
                    return Ok(false);
                }
            }
        }
        let new_capacity = capacity.saturating_mul(2).min(1024 * 1024);
        buffer
            .try_reserve_exact(new_capacity)
            .map_err(io::Error::other)?;
    }
}

/// BSD and macOS use libc's buffered directory stream on the deletion worker.
/// There is no completion-based unlink API there; each batch remains bounded,
/// cancellable between calls and anchored to the same retained descriptors.
#[cfg(all(unix, any(test, not(target_os = "linux"))))]
fn read_portable_directory_batch(
    frame: &mut DirectoryFrame,
    state: &WorkerState,
    path: &Path,
    errors: &mut Vec<DeleteError>,
) -> io::Result<bool> {
    use rustix::fs::{AtFlags, FileType, statat};
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;

    debug_assert!(frame.pending.is_empty());
    // Failed direct names already survive pending-child processing and reopen.
    frame.handle.prepare_buffered(state, frame.reopen_stream)?;
    frame.reopen_stream = false;
    #[cfg(test)]
    if let Some(hook) = state.read_hook.lock().unwrap().as_mut() {
        state.check_cancel()?;
        hook(&frame.name)?;
    }
    for _ in 0..256 {
        state.check_cancel()?;
        let entry = match frame.handle.entries_mut()?.read() {
            None => return Ok(true),
            Some(Ok(entry)) => entry,
            Some(Err(Errno::INTR)) => {
                state.check_cancel()?;
                // Dir permanently marks errors as exhausted. Its caller opens
                // a fresh stream for the one ordinary enumeration retry.
                return Err(io::Error::other("directory enumeration interrupted"));
            }
            Some(Err(error)) => return Err(error.into()),
        };
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(bytes);
        if frame
            .retry_skip
            .as_ref()
            .is_some_and(|skipped| skipped.contains(name))
        {
            continue;
        }
        state.examined();
        let fd = frame.handle.fd()?;
        let kind = entry.file_type();
        let mut is_directory = kind == FileType::Directory;
        if !is_directory && let Err(error) = unlink_entry(fd, name, false, state) {
            let mut captured = None;
            if kind == FileType::Unknown
                && (error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(Errno::ISDIR.raw_os_error()))
            {
                let inspected =
                    retry_operation(state, || statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)).or_else(
                        |error| {
                            if error.kind() != io::ErrorKind::PermissionDenied {
                                return Err(error);
                            }
                            repair_directory_permissions(fd, state, true)?;
                            retry_operation(state, || statat(fd, name, AtFlags::SYMLINK_NOFOLLOW))
                        },
                    );
                match inspected {
                    Ok(stat) => {
                        is_directory = FileType::from_raw_mode(stat.st_mode) == FileType::Directory;
                        captured = Some(FileIdentity::from_stat(&stat));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        record_error(errors, path.join(name), error)?;
                        remember_failed_child(&mut frame.retry_skip, name);
                        continue;
                    }
                }
            }
            if !is_directory
                && let Err(error) = retry_failed_unlink(
                    fd,
                    name,
                    false,
                    captured,
                    entry.ino(),
                    false,
                    state,
                    error,
                    None,
                )
            {
                record_error(errors, path.join(name), error)?;
                remember_failed_child(&mut frame.retry_skip, name);
            }
        }
        if is_directory {
            frame.pending.push(DirectoryName {
                name: name.to_owned(),
                inode: entry.ino(),
            });
        }
    }
    Ok(false)
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

    #[cfg(unix)]
    fn fixture() -> tempfile::TempDir {
        std::fs::create_dir_all(".tmp").unwrap();
        tempfile::Builder::new()
            .prefix("delete-")
            .tempdir_in(".tmp")
            .unwrap()
    }

    #[cfg(unix)]
    fn portable_state() -> WorkerState {
        WorkerState {
            #[cfg(target_os = "linux")]
            portable_enumeration: true,
            ..WorkerState::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn deletion_ancestors_reject_cycles_but_allow_devices_and_reused_sibling_inodes() {
        let identity = |device, inode| FileIdentity {
            device,
            inode,
            kind: rustix::fs::FileType::Directory,
        };
        let root = identity(1, 10);
        let child = identity(1, 11);
        let other_device = identity(2, 10);
        let mut ancestors = std::collections::HashSet::new();
        enter_directory(&mut ancestors, root).unwrap();
        enter_directory(&mut ancestors, child).unwrap();
        assert!(enter_directory(&mut ancestors, root).is_err());
        assert!(enter_directory(&mut ancestors, child).is_err());
        assert_eq!(ancestors.len(), 2);
        enter_directory(&mut ancestors, other_device).unwrap();
        ancestors.remove(&(child.device, child.inode));
        // A sibling created after removal may reuse the previous child's inode.
        enter_directory(&mut ancestors, child).unwrap();
        ancestors.remove(&(child.device, child.inode));
        ancestors.remove(&(other_device.device, other_device.inode));
        ancestors.remove(&(root.device, root.inode));
        // The one permitted parent rescan admits its retained frame again.
        enter_directory(&mut ancestors, root).unwrap();
        assert!(enter_directory(&mut ancestors, root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn selected_ancestor_alias_is_rejected_before_permission_repair() {
        use std::os::unix::fs::PermissionsExt;
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("retained"), b"keep").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let stat =
            rustix::fs::statat(&parent, "target", rustix::fs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let identity = FileIdentity::from_stat(&stat);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o0)).unwrap();
        let state = WorkerState::default();
        let mut errors = Vec::new();
        let mut removed = false;
        let mut path = std::path::absolute(&target).unwrap();
        // A bind alias has the same identity as one of the logical ancestors.
        let result = remove_from_parent(
            parent.as_fd(),
            std::ffi::OsStr::new("target"),
            &state,
            None,
            &mut path,
            &mut errors,
            &mut removed,
            || {
                Ok(std::collections::HashSet::from([(
                    identity.device,
                    identity.inode,
                )]))
            },
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.unwrap_err().to_string().contains("active ancestor"));
        assert!(!removed && errors.is_empty());
        assert_eq!(
            mode, 0,
            "an ancestor must never receive target permission repair"
        );
        assert_eq!(std::fs::read(target.join("retained")).unwrap(), b"keep");
        assert_eq!(state.snapshot().removed_entries, 0);
    }

    #[cfg(unix)]
    #[test]
    fn seeded_ancestor_child_is_skipped_once_while_siblings_are_deleted() {
        let work = fixture();
        let target = std::path::absolute(work.path()).unwrap().join("target");
        std::fs::create_dir_all(target.join("alias")).unwrap();
        std::fs::create_dir_all(target.join("ordinary")).unwrap();
        std::fs::write(target.join("alias/retained"), b"keep").unwrap();
        std::fs::write(target.join("ordinary/leaf"), b"remove").unwrap();
        std::fs::write(target.join("sibling"), b"remove").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let alias = rustix::fs::statat(
            &parent,
            "target/alias",
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
        let alias = FileIdentity::from_stat(&alias);
        let state = portable_state();
        let mut errors = Vec::new();
        let mut removed = false;
        let mut path = target.clone();
        remove_from_parent(
            parent.as_fd(),
            std::ffi::OsStr::new("target"),
            &state,
            None,
            &mut path,
            &mut errors,
            &mut removed,
            || {
                Ok(std::collections::HashSet::from([(
                    alias.device,
                    alias.inode,
                )]))
            },
        )
        .unwrap();
        assert!(!removed);
        assert!(!target.join("ordinary").exists());
        assert!(!target.join("sibling").exists());
        assert_eq!(
            std::fs::read(target.join("alias/retained")).unwrap(),
            b"keep"
        );
        assert_eq!(
            errors.len(),
            1,
            "the alias must not be revisited on the nonempty retry"
        );
        assert_eq!(errors[0].path, target.join("alias"));
        assert!(errors[0].message.contains("active ancestor"));
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn unreadable_ancestor_child_is_not_permission_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let stat =
            rustix::fs::statat(&parent, "target", rustix::fs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let identity = FileIdentity::from_stat(&stat);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o0)).unwrap();
        // With no expected identity, an unprivileged open reaches the held-path
        // permission branch. Privileged tests exercise the ordinary-open guard.
        let result = open_checked_directory(
            parent.as_fd(),
            "target".into(),
            None,
            0,
            &WorkerState::default(),
            &std::collections::HashSet::from([(identity.device, identity.inode)]),
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("an unreadable ancestor was admitted for deletion"),
        };
        assert!(error.to_string().contains("active ancestor"));
        assert_eq!(mode, 0);
    }

    #[cfg(unix)]
    #[test]
    fn leaf_deletion_never_collects_directory_ancestors() {
        let work = fixture();
        std::fs::write(work.path().join("leaf"), b"remove").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("leaf")).unwrap();
        let mut path = std::path::absolute(work.path()).unwrap().join("leaf");
        let mut errors = Vec::new();
        let mut removed = false;
        remove_from_parent(
            parent.as_fd(),
            std::ffi::OsStr::new("leaf"),
            &WorkerState::default(),
            None,
            &mut path,
            &mut errors,
            &mut removed,
            || panic!("leaf deletion must not open or inspect its ancestor chain"),
        )
        .unwrap();
        assert!(removed && errors.is_empty());
        assert!(!work.path().join("leaf").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cycle_diagnostic_prevents_rescan_and_keeps_sibling_deletion_running() {
        let work = fixture();
        let target = std::path::absolute(work.path()).unwrap().join("target");
        std::fs::create_dir_all(target.join("cycle")).unwrap();
        std::fs::write(target.join("cycle/retained"), b"keep").unwrap();
        std::fs::write(target.join("sibling"), b"remove").unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let state = portable_state();
        let mut frame = open_checked_directory(
            parent.as_fd(),
            "target".into(),
            None,
            0,
            &state,
            &std::collections::HashSet::new(),
        )
        .unwrap()
        .unwrap();
        let mut ancestors = std::collections::HashSet::new();
        enter_directory(&mut ancestors, frame.captured).unwrap();
        // Simulate metadata from a bind mount or directory hardlink to the root.
        let error = enter_directory(&mut ancestors, frame.captured).unwrap_err();
        let mut errors = Vec::new();
        record_error(&mut errors, target.join("cycle"), error).unwrap();
        // Simulate the direct-child failure bookkeeping as well as its report.
        remember_failed_child(&mut frame.retry_skip, std::ffi::OsStr::new("cycle"));
        assert!(prepare_directory_retry(&mut frame, parent.as_fd(), &state).unwrap());
        while !read_portable_directory_batch(&mut frame, &state, &target, &mut errors).unwrap() {}
        assert!(
            frame.pending.is_empty(),
            "the cycle must not be queued a second time"
        );
        assert!(!target.join("sibling").exists());
        assert_eq!(
            std::fs::read(target.join("cycle/retained")).unwrap(),
            b"keep"
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, target.join("cycle"));
        assert!(errors[0].path.is_absolute());
        assert!(errors[0].message.contains("active ancestor"));
        assert_eq!(state.snapshot().removed_entries, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portable_stream_retains_one_descriptor_per_directory() {
        use std::os::unix::fs::MetadataExt;

        fn descriptor_count(path: &Path) -> usize {
            let expected = std::fs::metadata(path).unwrap();
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::metadata(entry.path()).ok())
                .filter(|metadata| {
                    metadata.dev() == expected.dev() && metadata.ino() == expected.ino()
                })
                .count()
        }

        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let state = portable_state();
        let mut frame = open_checked_directory(
            parent.as_fd(),
            "target".into(),
            None,
            0,
            &state,
            &std::collections::HashSet::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(descriptor_count(&target), 1);
        state.cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            frame
                .handle
                .prepare_buffered(&state, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(matches!(frame.handle, DirectoryHandle::Raw(_)));
        assert_eq!(descriptor_count(&target), 1);
        state.cancel.store(false, Ordering::Relaxed);
        frame.handle.prepare_buffered(&state, false).unwrap();
        assert!(matches!(frame.handle, DirectoryHandle::Buffered(_)));
        assert_eq!(descriptor_count(&target), 1);
        state.cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            frame
                .handle
                .prepare_buffered(&state, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(descriptor_count(&target), 1);
        state.cancel.store(false, Ordering::Relaxed);
        frame.handle.prepare_buffered(&state, true).unwrap();
        assert_eq!(descriptor_count(&target), 1);
        drop(frame);
        assert_eq!(descriptor_count(&target), 0);
    }

    #[cfg(unix)]
    #[test]
    fn portable_stream_removes_wide_trees_without_following_symlinks() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(work.path().join("sentinel"), b"keep").unwrap();
        symlink("../sentinel", target.join("link")).unwrap();
        let raw = std::ffi::OsString::from_vec(vec![b'x', 0xff]);
        std::fs::write(target.join(raw), b"remove").unwrap();
        for index in 0..700 {
            std::fs::write(target.join(format!("leaf-{index}")), b"remove").unwrap();
        }
        for index in 0..300 {
            let directory = target.join(format!("dir-{index}"));
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("child"), b"remove").unwrap();
        }
        let anchor = RootAnchor::open(work.path()).unwrap();
        let state = portable_state();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
        assert!(removal.target_removed && removal.errors.is_empty());
        assert!(!target.exists());
        assert_eq!(
            std::fs::read(work.path().join("sentinel")).unwrap(),
            b"keep"
        );
        assert_eq!(state.snapshot().removed_entries, 1303);
        assert_eq!(state.snapshot().removed_directories, 301);
    }

    #[cfg(unix)]
    #[test]
    fn portable_stream_reopen_never_retries_exhausted_children() {
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        for index in 0..768 {
            std::fs::write(target.join(format!("leaf-{index}")), b"remove").unwrap();
        }
        let state = portable_state();
        let first_name = Arc::new(std::sync::Mutex::new(None::<std::ffi::OsString>));
        let failed_name = Arc::clone(&first_name);
        let failed_attempts = Arc::new(AtomicU64::new(0));
        let observed_attempts = Arc::clone(&failed_attempts);
        *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, directory| {
            if !directory {
                let mut first = first_name.lock().unwrap();
                let first = first.get_or_insert_with(|| name.to_owned());
                if first.as_os_str() == name {
                    observed_attempts.fetch_add(1, Ordering::Relaxed);
                    return Err(rustix::io::Errno::IO.into());
                }
            }
            Ok(())
        }));
        let read_calls = Arc::new(AtomicU64::new(0));
        let observed_reads = Arc::clone(&read_calls);
        *state.read_hook.lock().unwrap() = Some(Box::new(move |_| {
            if observed_reads.fetch_add(1, Ordering::Relaxed) == 1 {
                return Err(rustix::io::Errno::IO.into());
            }
            Ok(())
        }));
        let anchor = RootAnchor::open(work.path()).unwrap();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert!(removal.outcome.is_err());
        assert!(!removal.target_removed);
        assert_eq!(failed_attempts.load(Ordering::Relaxed), 2);
        assert_eq!(removal.errors.len(), 1);
        assert_eq!(state.snapshot().removed_entries, 767);
        let failed_name = failed_name.lock().unwrap().clone().unwrap();
        assert_eq!(
            removal.errors[0].path,
            std::path::absolute(target.join(&failed_name)).unwrap()
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
        assert!(target.join(failed_name).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn portable_stream_cancellation_keeps_completed_progress() {
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        for index in 0..8 {
            std::fs::write(target.join(format!("leaf-{index}")), b"remove").unwrap();
        }
        let state = portable_state();
        state.cancel_after.store(2, Ordering::Relaxed);
        let anchor = RootAnchor::open(work.path()).unwrap();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert_eq!(
            removal.outcome.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!removal.target_removed);
        assert_eq!(state.snapshot().removed_entries, 2);
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 6);
        assert!(removal.parent_metadata.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_notification_follows_result_and_finished_publication() {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let work = fixture();
        std::fs::write(work.path().join("target"), b"remove").unwrap();
        let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
        let target = capture(&anchor, Path::new("target")).unwrap();
        let completion = crate::completion::Completion::new().unwrap();
        let handle = spawn_with_completion(
            anchor,
            PathBuf::from("target"),
            Some(target),
            completion.wake(),
        );
        let completion_fd = completion.as_fd();
        let mut fds = [PollFd::new(&completion_fd, PollFlags::IN)];
        assert_eq!(
            poll(
                &mut fds,
                Some(&Timespec {
                    tv_sec: 5,
                    tv_nsec: 0
                })
            )
            .unwrap(),
            1
        );
        assert!(handle.is_finished());
        assert!(handle.receiver.try_recv().unwrap().outcome.is_ok());
        let mut count = [0_u8; 8];
        assert_eq!(rustix::io::read(completion.as_fd(), &mut count).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(count), 1);
        assert_eq!(
            rustix::io::read(completion.as_fd(), &mut count),
            Err(rustix::io::Errno::AGAIN)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_guard_wakes_on_unwind_and_never_waits_on_a_full_counter() {
        let completion = crate::completion::Completion::new().unwrap();
        let state = Arc::new(WorkerState::default());
        let wake = completion.wake();
        let (sender, receiver) = bounded(1);
        let result = std::panic::catch_unwind(|| {
            let _guard = FinishedGuard {
                state: Arc::clone(&state),
                completion: wake.clone(),
                sender: Some(sender),
            };
            panic!("completion guard unwind fixture");
        });
        assert!(result.is_err());
        assert!(matches!(
            receiver.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
        assert!(state.finished.load(Ordering::Acquire));
        let mut count = [0_u8; 8];
        assert_eq!(rustix::io::read(completion.as_fd(), &mut count).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(count), 1);
        rustix::io::write(completion.as_fd(), &(u64::MAX - 1).to_ne_bytes()).unwrap();
        state.finished.store(false, Ordering::Relaxed);
        drop(FinishedGuard {
            state: Arc::clone(&state),
            completion: wake,
            sender: None,
        });
        assert!(state.finished.load(Ordering::Acquire));
        assert_eq!(rustix::io::read(completion.as_fd(), &mut count).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(count), u64::MAX - 1);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_between_unlinks_retains_progress_without_more_metadata() {
        let work = fixture();
        std::fs::create_dir(work.path().join("target")).unwrap();
        for n in 0..8 {
            std::fs::write(work.path().join("target").join(format!("file-{n}")), b"x").unwrap();
        }
        let anchor = RootAnchor::open(work.path()).unwrap();
        let state = WorkerState::default();
        state.cancel_after.store(2, Ordering::Relaxed);
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert!(!removal.target_removed);
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
        assert!(removal.parent_metadata.is_none());
        assert!(removal.metadata_error.is_none());
        let resumed = WorkerState::default();
        remove_confirmed(&anchor, Path::new("target"), &resumed)
            .unwrap()
            .outcome
            .unwrap();
        assert!(!work.path().join("target").exists());
        assert_eq!(resumed.snapshot().removed_entries, 7);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_after_final_unlink_preserves_success_without_parent_metadata() {
        for directory in [false, true] {
            let work = fixture();
            let target = work.path().join("target");
            if directory {
                std::fs::create_dir(&target).unwrap();
            } else {
                std::fs::write(&target, b"remove").unwrap();
            }
            let anchor = RootAnchor::open(work.path()).unwrap();
            let state = WorkerState::default();
            state.cancel_after.store(1, Ordering::Relaxed);
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert!(removal.target_removed);
            assert!(removal.outcome.is_ok());
            assert!(!target.exists());
            assert_eq!(state.snapshot().removed_entries, 1);
            assert_eq!(state.snapshot().removed_directories, u64::from(directory));
            assert!(removal.parent_metadata.is_none() && removal.metadata_error.is_none());
        }
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn failures_are_collected_with_full_paths_while_siblings_continue() {
        let work = fixture();
        std::fs::create_dir_all(work.path().join("target/nested/deeper")).unwrap();
        std::fs::create_dir(work.path().join("target/removable")).unwrap();
        let state = WorkerState::default();
        let mut expected_paths = std::collections::HashSet::new();
        let attempts = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&attempts);
        *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, _| {
            if name.as_encoded_bytes().starts_with(b"failure-") {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }));
        for n in 0..512 {
            let relative = format!("target/nested/deeper/failure-{n}");
            let path = work.path().join(&relative);
            std::fs::write(&path, b"keep").unwrap();
            state
                .failed_names
                .lock()
                .unwrap()
                .insert(path.file_name().unwrap().to_owned());
            expected_paths.insert(std::path::absolute(path).unwrap());
            std::fs::write(
                work.path().join("target/removable").join(format!("ok-{n}")),
                b"remove",
            )
            .unwrap();
        }
        let anchor = RootAnchor::open(work.path()).unwrap();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert!(removal.outcome.is_err());
        assert_eq!(removal.errors.len(), 512);
        assert_eq!(attempts.load(Ordering::Relaxed), 1024);
        for error in removal.errors {
            assert!(error.path.is_absolute());
            assert!(
                expected_paths.remove(&error.path),
                "unexpected or duplicate error: {error:?}"
            );
            assert!(error.message.contains("injected removal failure"));
        }
        assert!(expected_paths.is_empty());
        assert!(!work.path().join("target/removable").exists());
        assert_eq!(state.snapshot().removed_entries, 513);
        assert!(work.path().join("target/nested/deeper").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn deep_wide_failures_preserve_attempt_limits_and_remove_every_healthy_sibling() {
        const DEPTH: usize = 24;
        const FAILURES: usize = 384;
        for state in [WorkerState::default(), portable_state()] {
            let work = fixture();
            let target = work.path().join("target");
            std::fs::create_dir(&target).unwrap();
            let mut ancestors = vec![target.clone()];
            for level in 0..DEPTH {
                let directory = ancestors.last().unwrap().join(format!("level-{level:02}"));
                std::fs::create_dir(&directory).unwrap();
                ancestors.push(directory);
            }
            let mut healthy = Vec::new();
            for (level, directory) in ancestors.iter().enumerate() {
                let file = directory.join(format!("healthy-file-{level:02}"));
                std::fs::write(&file, b"remove").unwrap();
                healthy.push(file);
                let sibling = directory.join(format!("healthy-dir-{level:02}"));
                std::fs::create_dir(&sibling).unwrap();
                std::fs::write(sibling.join("payload"), b"remove").unwrap();
                healthy.push(sibling);
            }
            let deepest = ancestors.last().unwrap();
            let mut expected_paths = std::collections::HashSet::new();
            for index in 0..FAILURES {
                let name = std::ffi::OsString::from(format!("failure-{index:03}"));
                let path = deepest.join(&name);
                std::fs::write(&path, b"keep").unwrap();
                state.failed_names.lock().unwrap().insert(name);
                expected_paths.insert(std::path::absolute(path).unwrap());
            }
            let attempts = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let observed = Arc::clone(&attempts);
            *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, directory| {
                *observed
                    .lock()
                    .unwrap()
                    .entry((name.to_owned(), directory))
                    .or_insert(0_usize) += 1;
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert!(removal.outcome.is_err() && !removal.target_removed);
            assert_eq!(removal.errors.len(), FAILURES);
            let attempts = attempts.lock().unwrap();
            for error in &removal.errors {
                assert!(error.path.is_absolute());
                assert!(
                    expected_paths.remove(&error.path),
                    "duplicate or unexpected error: {error:?}"
                );
                assert!(error.message.contains("injected removal failure"));
                assert_eq!(
                    attempts.get(&(error.path.file_name().unwrap().to_owned(), false)),
                    Some(&2),
                    "ancestor retries repeated an exhausted leaf"
                );
                assert_eq!(std::fs::read(&error.path).unwrap(), b"keep");
            }
            assert!(expected_paths.is_empty());
            for directory in &ancestors {
                assert!(directory.is_dir());
                assert_eq!(
                    attempts.get(&(directory.file_name().unwrap().to_owned(), true)),
                    Some(&2),
                    "each retained ancestor gets just one rescan"
                );
            }
            for path in healthy {
                assert!(!path.exists(), "healthy sibling was left behind: {path:?}");
            }
            assert_eq!(state.snapshot().removed_entries, ((DEPTH + 1) * 3) as u64);
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolved_errors_do_not_skip_a_replacement_child_during_parent_retry() {
        const RESOLVED_FAILURES: usize = 32;
        for keep_failed_sibling in [false, true] {
            for state in [WorkerState::default(), portable_state()] {
                let work = fixture();
                let target = work.path().join("target");
                let recovered = target.join("recovered");
                std::fs::create_dir_all(&recovered).unwrap();
                let mut vanished = Vec::new();
                for index in 0..RESOLVED_FAILURES {
                    let name = std::ffi::OsString::from(format!("vanished-{index:02}"));
                    let path = recovered.join(&name);
                    std::fs::write(&path, b"removed externally").unwrap();
                    state.failed_names.lock().unwrap().insert(name);
                    vanished.push(path);
                }
                if keep_failed_sibling {
                    std::fs::write(target.join("retained"), b"keep").unwrap();
                    state.failed_names.lock().unwrap().insert("retained".into());
                }
                let directory_attempts = Arc::new(std::sync::Mutex::new((0_usize, 0_usize)));
                let observed = Arc::clone(&directory_attempts);
                let replacement = recovered.clone();
                *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, directory| {
                    if !directory {
                        return Ok(());
                    }
                    let mut attempts = observed.lock().unwrap();
                    if name == "recovered" {
                        attempts.0 += 1;
                        if attempts.0 == 1 {
                            for path in &vanished {
                                std::fs::remove_file(path)?;
                            }
                        }
                    } else if name == "target" {
                        attempts.1 += 1;
                        if attempts.1 == 1 {
                            // The first incarnation is gone and its errors were
                            // truncated. This new child must remain eligible.
                            std::fs::create_dir(&replacement)?;
                            std::fs::write(replacement.join("new-payload"), b"remove")?;
                        }
                    }
                    Ok(())
                }));
                let anchor = RootAnchor::open(work.path()).unwrap();
                let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
                assert_eq!(*directory_attempts.lock().unwrap(), (2, 2));
                assert!(
                    !recovered.exists(),
                    "a resolved child poisoned the parent's retry skip set"
                );
                assert_eq!(removal.target_removed, !keep_failed_sibling);
                assert_eq!(removal.outcome.is_err(), keep_failed_sibling);
                assert_eq!(removal.errors.len(), usize::from(keep_failed_sibling));
                if keep_failed_sibling {
                    assert_eq!(
                        removal.errors[0].path,
                        std::path::absolute(target.join("retained")).unwrap()
                    );
                    assert!(
                        removal.errors[0]
                            .message
                            .contains("injected removal failure")
                    );
                    assert_eq!(std::fs::read(target.join("retained")).unwrap(), b"keep");
                } else {
                    assert!(!target.exists());
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_read_errors_are_reported_only_after_one_retry() {
        for persistent in [false, true] {
            let work = fixture();
            std::fs::create_dir_all(work.path().join("target/flaky")).unwrap();
            std::fs::write(work.path().join("target/flaky/file"), b"remove").unwrap();
            std::fs::create_dir(work.path().join("target/sibling")).unwrap();
            let failures = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&failures);
            let state = WorkerState::default();
            *state.read_hook.lock().unwrap() = Some(Box::new(move |name| {
                if name == "flaky" && (persistent || observed.load(Ordering::Relaxed) == 0) {
                    observed.fetch_add(1, Ordering::Relaxed);
                    return Err(rustix::io::Errno::IO.into());
                }
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert_eq!(removal.outcome.is_err(), persistent);
            assert_eq!(removal.target_removed, !persistent);
            assert_eq!(removal.errors.len(), usize::from(persistent));
            assert_eq!(
                failures.load(Ordering::Relaxed),
                if persistent { 2 } else { 1 }
            );
            assert!(!work.path().join("target/sibling").exists());
            if persistent {
                assert_eq!(
                    removal.errors[0].path,
                    std::path::absolute(work.path().join("target/flaky")).unwrap()
                );
                assert!(work.path().join("target/flaky/file").is_file());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn removed_empty_directory_does_not_report_read_failure() {
        for nested in [false, true] {
            let work = fixture();
            let relative = if nested { "target/flaky" } else { "flaky" };
            std::fs::create_dir_all(work.path().join(relative)).unwrap();
            if nested {
                std::fs::create_dir(work.path().join("target/sibling")).unwrap();
            }
            let failures = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&failures);
            let state = WorkerState::default();
            *state.read_hook.lock().unwrap() = Some(Box::new(move |name| {
                if name == "flaky" {
                    observed.fetch_add(1, Ordering::Relaxed);
                    return Err(rustix::io::Errno::IO.into());
                }
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let requested = if nested { "target" } else { "flaky" };
            let removal = remove_confirmed(&anchor, Path::new(requested), &state).unwrap();
            assert_eq!(failures.load(Ordering::Relaxed), 2);
            assert!(removal.outcome.is_ok());
            assert!(removal.target_removed);
            assert!(removal.errors.is_empty());
            assert!(!work.path().join(requested).exists());
            assert!(work.path().is_dir());
        }
    }

    #[cfg(unix)]
    #[test]
    fn transient_unlink_failures_get_exactly_one_retry() {
        for directory in [false, true] {
            for nested in [false, true] {
                let work = fixture();
                let relative = if nested { "target/flaky" } else { "flaky" };
                if nested {
                    std::fs::create_dir(work.path().join("target")).unwrap();
                }
                if directory {
                    std::fs::create_dir(work.path().join(relative)).unwrap();
                } else {
                    std::fs::write(work.path().join(relative), b"remove").unwrap();
                }
                let attempts = Arc::new(AtomicU64::new(0));
                let observed = Arc::clone(&attempts);
                let state = WorkerState::default();
                *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, _| {
                    if name == "flaky" && observed.fetch_add(1, Ordering::Relaxed) == 0 {
                        return Err(rustix::io::Errno::IO.into());
                    }
                    Ok(())
                }));
                let anchor = RootAnchor::open(work.path()).unwrap();
                let requested = if nested { "target" } else { "flaky" };
                let removal = remove_confirmed(&anchor, Path::new(requested), &state).unwrap();
                assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
                assert!(removal.target_removed && removal.errors.is_empty());
                assert_eq!(attempts.load(Ordering::Relaxed), 2);
                assert!(!work.path().join(requested).exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn nonempty_directory_is_rescanned_once_without_following_links() {
        use std::os::unix::fs::symlink;
        for nested in [false, true] {
            let work = fixture();
            let relative = if nested { "target/changing" } else { "target" };
            let directory = work.path().join(relative);
            std::fs::create_dir_all(&directory).unwrap();
            let sentinel = std::path::absolute(work.path().join("sentinel")).unwrap();
            std::fs::write(&sentinel, b"keep").unwrap();
            let attempts = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&attempts);
            let state = WorkerState::default();
            *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, is_directory| {
                if is_directory
                    && name == directory.file_name().unwrap()
                    && observed.fetch_add(1, Ordering::Relaxed) == 0
                {
                    std::fs::create_dir_all(directory.join("late/nested"))?;
                    std::fs::write(directory.join("late/nested/file"), b"remove")?;
                    symlink(&sentinel, directory.join("late/link"))?;
                }
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
            assert!(removal.target_removed && removal.errors.is_empty());
            assert_eq!(attempts.load(Ordering::Relaxed), 2);
            assert!(!work.path().join("target").exists());
            assert_eq!(
                std::fs::read(work.path().join("sentinel")).unwrap(),
                b"keep"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_second_nonempty_failure_keeps_only_that_subtree_and_finishes_siblings() {
        let work = fixture();
        let changing = work.path().join("target/changing");
        std::fs::create_dir_all(&changing).unwrap();
        for n in 0..9 {
            let sibling = work.path().join(format!("target/sibling-{n}"));
            std::fs::create_dir(&sibling).unwrap();
            std::fs::write(sibling.join("file"), b"remove").unwrap();
        }
        let attempts = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&attempts);
        let state = WorkerState::default();
        *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, directory| {
            if directory && name == "changing" {
                let attempt = observed.fetch_add(1, Ordering::Relaxed);
                let late = changing.join(format!("late-{attempt}"));
                std::fs::create_dir(&late)?;
                std::fs::write(late.join("file"), b"remove")?;
            }
            Ok(())
        }));
        let anchor = RootAnchor::open(work.path()).unwrap();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert!(removal.outcome.is_err() && !removal.target_removed);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert_eq!(removal.errors.len(), 1);
        assert_eq!(
            removal.errors[0].path,
            std::path::absolute(work.path().join("target/changing")).unwrap()
        );
        assert!(!work.path().join("target/changing/late-0").exists());
        assert!(work.path().join("target/changing/late-1/file").is_file());
        for n in 0..9 {
            assert!(!work.path().join(format!("target/sibling-{n}")).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn permission_and_nonempty_failures_share_the_two_unlink_attempt_limit() {
        for permission_first in [false, true] {
            let work = fixture();
            let directory = work.path().join("target");
            std::fs::create_dir(&directory).unwrap();
            let attempts = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&attempts);
            let state = WorkerState::default();
            *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, is_directory| {
                if is_directory && name == "target" {
                    let attempt = observed.fetch_add(1, Ordering::Relaxed);
                    if (attempt == 0) == permission_first {
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                    std::fs::write(directory.join("late"), b"remove")?;
                }
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert!(removal.outcome.is_err() && !removal.target_removed);
            assert_eq!(attempts.load(Ordering::Relaxed), 2);
            assert_eq!(removal.errors.len(), 1);
            assert_eq!(work.path().join("target/late").exists(), permission_first);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_during_a_directory_rescan_stops_before_the_second_rmdir() {
        let work = fixture();
        let directory = work.path().join("target");
        std::fs::create_dir(&directory).unwrap();
        let attempts = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&attempts);
        let state = WorkerState::default();
        state.cancel_after.store(1, Ordering::Relaxed);
        *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, is_directory| {
            if is_directory && name == "target" {
                observed.fetch_add(1, Ordering::Relaxed);
                std::fs::write(directory.join("late"), b"remove")?;
            }
            Ok(())
        }));
        let anchor = RootAnchor::open(work.path()).unwrap();
        let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
        assert_eq!(
            removal.outcome.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!removal.target_removed && removal.errors.is_empty());
        assert!(removal.parent_metadata.is_none());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(state.snapshot().removed_entries, 1);
        assert!(work.path().join("target").is_dir());
        assert!(!work.path().join("target/late").exists());
    }

    #[cfg(unix)]
    #[test]
    fn external_parent_setgid_recovery_requires_effective_or_supplementary_membership() {
        use rustix::fs::Mode;
        let mode = Mode::from_raw_mode(0o2500);
        ensure_parent_mode_recoverable(mode, 17, 17, || panic!("effective group matches")).unwrap();
        ensure_parent_mode_recoverable(mode, 17, 23, || Ok(true)).unwrap();
        let error = ensure_parent_mode_recoverable(mode, 17, 23, || Ok(false)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("setgid"));
        let error = ensure_parent_mode_recoverable(mode, 17, 23, || {
            Err(io::Error::other("group lookup failed"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("group lookup failed"));
        ensure_parent_mode_recoverable(Mode::from_raw_mode(0o500), 17, 23, || {
            panic!("ordinary modes need no group lookup")
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restored_mode_verification_detects_silent_special_bit_loss() {
        use rustix::fs::Mode;
        let work = fixture();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let fd = anchor.open_directory(Path::new("")).unwrap();
        let state = WorkerState::default();
        // A successful chmod alone does not establish the requested final mode.
        let actual = Mode::from_raw_mode(0o500);
        chmod_held(fd.as_fd(), actual, &state).unwrap();
        verify_held_mode(fd.as_fd(), actual, &state).unwrap();
        for expected in [0o2500, 0o4500, 0o1500, 0o700] {
            let error =
                verify_held_mode(fd.as_fd(), Mode::from_raw_mode(expected), &state).unwrap_err();
            assert!(error.to_string().contains("permission restoration"));
            assert!(error.to_string().contains(&format!("{expected:04o}")));
            assert!(error.to_string().contains("0500"));
        }
        chmod_held(fd.as_fd(), Mode::RWXU, &state).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_setgid_parent_restores_after_removal_and_skips_unchanged_modes() {
        use std::os::unix::fs::PermissionsExt;
        for original_mode in [0o2500, 0o2700] {
            for cancel_after_unlink in [false, true] {
                let work = fixture();
                let parent = work.path().join("parent");
                std::fs::create_dir(&parent).unwrap();
                // An owner can assign an owned fixture to their effective group.
                rustix::fs::chown(&parent, None, Some(rustix::process::getegid())).unwrap();
                let target = parent.join("target");
                std::fs::write(&target, b"remove").unwrap();
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(original_mode))
                    .unwrap();
                let anchor = RootAnchor::open(work.path()).unwrap();
                let expected = capture(&anchor, Path::new("parent/target")).unwrap();
                let state = WorkerState::default();
                state.deny_next_unlink.store(true, Ordering::Relaxed);
                // A no-op repair must not attempt restoration at all.
                state
                    .fail_parent_restore
                    .store(original_mode == 0o2700, Ordering::Relaxed);
                if cancel_after_unlink {
                    state.cancel_after.store(1, Ordering::Relaxed);
                }
                let removal =
                    remove_expected(&anchor, Path::new("parent/target"), &state, Some(expected))
                        .unwrap();
                let restored = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o7777;
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
                assert!(removal.target_removed);
                assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
                assert!(removal.errors.is_empty());
                assert_eq!(state.snapshot().removed_entries, 1);
                assert!(!target.exists());
                assert_eq!(restored, original_mode);
                if cancel_after_unlink {
                    assert!(removal.parent_metadata.is_none());
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
    #[test]
    fn owned_directories_are_repaired_and_external_parent_mode_is_restored() {
        use std::os::unix::fs::PermissionsExt;
        let work = fixture();
        std::fs::write(work.path().join("outside"), b"keep").unwrap();
        std::fs::create_dir_all(work.path().join("target/locked")).unwrap();
        std::fs::write(work.path().join("target/locked/leaf"), b"remove").unwrap();
        std::fs::create_dir_all(work.path().join("target/no-search/child")).unwrap();
        std::fs::write(work.path().join("target/no-search/leaf"), b"remove").unwrap();
        std::fs::write(work.path().join("target/no-search/child/leaf"), b"remove").unwrap();
        std::fs::set_permissions(
            work.path().join("target/no-search"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        std::fs::set_permissions(
            work.path().join("target/locked"),
            std::fs::Permissions::from_mode(0o0),
        )
        .unwrap();
        std::fs::set_permissions(
            work.path().join("target"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        std::fs::set_permissions(work.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        #[cfg(target_os = "macos")]
        let locked_handle_available = {
            let parent = anchor.open_directory(Path::new("target")).unwrap();
            match checked_path_handle(
                parent.as_fd(),
                std::ffi::OsStr::new("locked"),
                None,
                0,
                &WorkerState::default(),
            ) {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
                Err(error) => panic!("unexpected event-only handle failure: {error}"),
            }
        };
        #[cfg(not(target_os = "macos"))]
        let locked_handle_available = true;
        let target = capture(&anchor, Path::new("target")).unwrap();
        let removal = remove_expected(
            &anchor,
            Path::new("target"),
            &WorkerState::default(),
            Some(target),
        )
        .unwrap();
        let mode = std::fs::metadata(work.path()).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(work.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(mode, 0o500);
        assert_eq!(std::fs::read(work.path().join("outside")).unwrap(), b"keep");
        assert!(!work.path().join("target/no-search").exists());
        if locked_handle_available {
            assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
            assert!(removal.errors.is_empty());
            assert!(!work.path().join("target").exists());
        } else {
            // Darwin may keep read-right checks for O_EVTONLY. This precise
            // capability failure must retain only the locked child, while the
            // rest of the requested subtree is still removed.
            assert!(removal.outcome.is_err());
            assert!(!removal.target_removed);
            assert_eq!(removal.errors.len(), 1);
            assert_eq!(
                removal.errors[0].path,
                std::path::absolute(work.path().join("target/locked")).unwrap()
            );
            let locked = work.path().join("target/locked");
            assert_eq!(
                std::fs::metadata(&locked).unwrap().permissions().mode() & 0o777,
                0
            );
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(std::fs::read(locked.join("leaf")).unwrap(), b"remove");
            assert_eq!(
                std::fs::read_dir(work.path().join("target"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn parent_restore_failure_preserves_successful_leaf_removal() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for link in [false, true] {
            for cancel_after_unlink in [false, true] {
                let work = fixture();
                let parent = work.path().join("parent");
                std::fs::create_dir(&parent).unwrap();
                std::fs::write(work.path().join("sentinel"), b"keep").unwrap();
                let target = parent.join("target");
                if link {
                    symlink("../sentinel", &target).unwrap();
                } else {
                    std::fs::write(&target, b"remove").unwrap();
                }
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
                let anchor = RootAnchor::open(work.path()).unwrap();
                let expected = capture(&anchor, Path::new("parent/target")).unwrap();
                let state = WorkerState::default();
                // Force recovery even when the test runner has elevated privileges.
                state.deny_next_unlink.store(true, Ordering::Relaxed);
                state.fail_parent_restore.store(true, Ordering::Relaxed);
                if cancel_after_unlink {
                    state.cancel_after.store(1, Ordering::Relaxed);
                }
                let removal =
                    remove_expected(&anchor, Path::new("parent/target"), &state, Some(expected))
                        .unwrap();
                assert!(removal.target_removed);
                assert!(removal.outcome.is_err());
                assert_eq!(state.snapshot().removed_entries, 1);
                assert!(std::fs::symlink_metadata(&target).is_err());
                assert_eq!(
                    std::fs::read(work.path().join("sentinel")).unwrap(),
                    b"keep"
                );
                assert_eq!(removal.errors.len(), 1);
                assert_eq!(
                    removal.errors[0].path,
                    std::path::absolute(&target).unwrap()
                );
                assert!(
                    removal.errors[0]
                        .message
                        .contains("could not restore permissions")
                );
                assert!(
                    removal.errors[0]
                        .message
                        .contains("injected parent restoration failure")
                );
                if cancel_after_unlink {
                    assert!(removal.parent_metadata.is_none());
                }
                // The injected failure intentionally leaves the recovered mode.
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn capture_does_not_require_parent_read_permission_or_change_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let work = fixture();
        std::fs::create_dir(work.path().join("parent")).unwrap();
        std::fs::write(work.path().join("parent/target"), b"remove").unwrap();
        std::fs::set_permissions(
            work.path().join("parent"),
            std::fs::Permissions::from_mode(0o300),
        )
        .unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let expected = capture(&anchor, Path::new("parent/target")).unwrap();
        let mode = std::fs::metadata(work.path().join("parent"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o300);
        let removal = remove_expected(
            &anchor,
            Path::new("parent/target"),
            &WorkerState::default(),
            Some(expected),
        )
        .unwrap();
        std::fs::set_permissions(
            work.path().join("parent"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(removal.outcome.is_ok());
        assert!(removal.errors.is_empty());
        assert!(!work.path().join("parent/target").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_symlinks_and_replacement_inodes() {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, statat};
        use std::os::unix::fs::{PermissionsExt, symlink};
        let work = fixture();
        std::fs::write(work.path().join("outside"), b"keep").unwrap();
        std::fs::set_permissions(
            work.path().join("outside"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        symlink("outside", work.path().join("target")).unwrap();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let parent = anchor.open_parent(Path::new("target")).unwrap();
        let link = openat(
            &parent,
            "target",
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        assert!(chmod_held(link.as_fd(), Mode::RWXU, &WorkerState::default()).is_err());
        let captured = statat(&parent, "target", AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::rename(work.path().join("target"), work.path().join("original")).unwrap();
        std::fs::write(work.path().join("target"), b"replacement").unwrap();
        let result = retry_failed_unlink(
            parent.as_fd(),
            std::ffi::OsStr::new("target"),
            false,
            Some(FileIdentity::from_stat(&captured)),
            0,
            true,
            &WorkerState::default(),
            io::Error::from(io::ErrorKind::PermissionDenied),
            None,
        );
        assert!(result.unwrap_err().to_string().contains("changed"));
        assert_eq!(
            std::fs::read(work.path().join("target")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::metadata(work.path().join("outside"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
    }

    #[cfg(unix)]
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
            parent.as_fd(),
            std::ffi::OsString::from("target"),
            Some(FileIdentity::from_stat(&captured)),
            0,
            &WorkerState::default(),
            &std::collections::HashSet::new(),
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(work.path().join("target/sentinel")).unwrap(),
            b"keep"
        );
        assert!(work.path().join("original").is_dir());
    }

    #[cfg(target_os = "linux")]
    fn synthetic_leaves(names: &[&str], state: &WorkerState) -> LeafUnlinks {
        for _ in names {
            state.examined();
        }
        let mut leaves = LeafUnlinks::default();
        for name in names {
            leaves.push(std::ffi::OsStr::new(name), 0);
        }
        leaves
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_leaf_batch_counts_all_completed_unlinks_before_stopping() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        let work = fixture();
        std::fs::write(work.path().join("waiting"), b"keep").unwrap();
        let fd = rustix::fs::open(
            work.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let state = WorkerState::default();
        let leaves = synthetic_leaves(&["removed-first", "removed-second", "waiting"], &state);
        state.cancel.store(true, Ordering::Relaxed);
        let mut errors = Vec::new();
        let mut skipped = None;
        let result = apply_leaf_completions(
            fd.as_fd(),
            &leaves,
            &state,
            work.path(),
            &mut errors,
            &mut skipped,
            BatchOutcome::Complete(vec![
                EntryOutcome::Completed(Ok(())),
                EntryOutcome::Completed(Ok(())),
                EntryOutcome::NotSubmitted,
            ]),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(state.snapshot().removed_entries, 2);
        assert!(work.path().join("waiting").exists());
        assert!(errors.is_empty());
        assert!(skipped.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn leaf_batch_interrupted_completion_retries_and_absence_does_not_count() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        let work = fixture();
        for name in ["interrupted", "sibling"] {
            std::fs::write(work.path().join(name), b"remove").unwrap();
        }
        let fd = rustix::fs::open(
            work.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let state = WorkerState::default();
        let leaves = synthetic_leaves(&["absent", "interrupted", "sibling"], &state);
        let mut errors = Vec::new();
        let mut skipped = None;
        apply_leaf_completions(
            fd.as_fd(),
            &leaves,
            &state,
            work.path(),
            &mut errors,
            &mut skipped,
            BatchOutcome::Complete(vec![
                EntryOutcome::Completed(Err(rustix::io::Errno::NOENT.into())),
                EntryOutcome::Completed(Err(rustix::io::Errno::INTR.into())),
                EntryOutcome::NotSubmitted,
            ]),
        )
        .unwrap();
        assert_eq!(state.snapshot().removed_entries, 2);
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
        assert!(errors.is_empty());
        assert!(skipped.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_leaf_batch_retries_once_and_continues_siblings() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        let work = fixture();
        for name in ["denied", "sibling"] {
            std::fs::write(work.path().join(name), b"entry").unwrap();
        }
        let fd = rustix::fs::open(
            work.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let state = WorkerState::default();
        state.failed_names.lock().unwrap().insert("denied".into());
        let attempts = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&attempts);
        *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, _| {
            if name == "denied" {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }));
        let leaves = synthetic_leaves(&["denied", "sibling"], &state);
        let mut errors = Vec::new();
        let mut skipped = None;
        apply_leaf_completions(
            fd.as_fd(),
            &leaves,
            &state,
            work.path(),
            &mut errors,
            &mut skipped,
            BatchOutcome::Complete(vec![
                EntryOutcome::Completed(Err(rustix::io::Errno::ACCESS.into())),
                EntryOutcome::NotSubmitted,
            ]),
        )
        .unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(state.snapshot().removed_entries, 1);
        assert!(work.path().join("denied").exists());
        assert!(!work.path().join("sibling").exists());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, work.path().join("denied"));
        let skipped = skipped.unwrap();
        assert_eq!(skipped.len(), 1);
        assert!(skipped.contains(std::ffi::OsStr::new("denied")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uncertain_leaf_completion_is_not_replayed_or_revisited_on_parent_retry() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        let work = fixture();
        let target = work.path().join("target");
        std::fs::create_dir(&target).unwrap();
        for name in ["uncertain", "sibling"] {
            std::fs::write(target.join(name), b"entry").unwrap();
        }
        let parent = rustix::fs::open(
            work.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let state = WorkerState::default();
        let mut frame = open_checked_directory(
            parent.as_fd(),
            "target".into(),
            None,
            0,
            &state,
            &std::collections::HashSet::new(),
        )
        .unwrap()
        .unwrap();
        let leaves = synthetic_leaves(&["uncertain", "absent", "sibling"], &state);
        let mut errors = Vec::new();
        apply_leaf_completions(
            frame.handle.fd().unwrap(),
            &leaves,
            &state,
            &target,
            &mut errors,
            &mut frame.retry_skip,
            BatchOutcome::Failed {
                entries: vec![
                    EntryOutcome::Unknown,
                    EntryOutcome::Unknown,
                    EntryOutcome::NotSubmitted,
                ],
                error: io::Error::other("injected driver failure"),
            },
        )
        .unwrap();
        assert_eq!(state.snapshot().removed_entries, 1);
        assert!(target.join("uncertain").exists());
        assert!(!target.join("sibling").exists());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, target.join("uncertain"));
        assert!(prepare_directory_retry(&mut frame, parent.as_fd(), &state).unwrap());
        let skipped = frame.retry_skip.as_ref().unwrap();
        assert!(skipped.contains(std::ffi::OsStr::new("uncertain")));
        assert!(!skipped.contains(std::ffi::OsStr::new("absent")));
    }

    #[cfg(unix)]
    #[test]
    fn successful_ancestor_removal_discards_only_its_resolved_descendant_errors() {
        for keep_failed_sibling in [false, true] {
            let work = fixture();
            let target = work.path().join("target");
            let recovered = target.join("recovered");
            std::fs::create_dir_all(&recovered).unwrap();
            let vanished = recovered.join("vanished");
            std::fs::write(&vanished, b"removed externally").unwrap();
            if keep_failed_sibling {
                std::fs::write(target.join("retained"), b"still present").unwrap();
            }
            let state = WorkerState::default();
            {
                let mut failed = state.failed_names.lock().unwrap();
                failed.insert("vanished".into());
                failed.insert("retained".into());
            }
            *state.unlink_hook.lock().unwrap() = Some(Box::new(move |name, directory| {
                if directory && name == "recovered" {
                    // Both attempts on this child failed. Another process can
                    // still remove it before its ancestor's successful rmdir.
                    std::fs::remove_file(&vanished)?;
                }
                Ok(())
            }));
            let anchor = RootAnchor::open(work.path()).unwrap();
            let removal = remove_confirmed(&anchor, Path::new("target"), &state).unwrap();
            assert!(!recovered.exists());
            if keep_failed_sibling {
                assert!(!removal.target_removed);
                assert!(removal.outcome.is_err());
                assert_eq!(removal.errors.len(), 1);
                assert_eq!(
                    removal.errors[0].path,
                    std::path::absolute(target.join("retained")).unwrap()
                );
            } else {
                assert!(removal.target_removed);
                assert!(removal.outcome.is_ok());
                assert!(removal.errors.is_empty());
                assert!(!target.exists());
            }
        }
    }
}
