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
    #[cfg(test)]
    failed_names: std::sync::Mutex<std::collections::HashSet<std::ffi::OsString>>,
    #[cfg(test)]
    deny_next_unlink: AtomicBool,
    #[cfg(test)]
    fail_parent_restore: AtomicBool,
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

    #[cfg(target_os = "linux")]
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

#[derive(Clone, Default)]
struct CompletionWake {
    #[cfg(target_os = "linux")]
    fd: Option<Arc<rustix::fd::OwnedFd>>,
}

impl CompletionWake {
    fn signal(&self) {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            let increment = 1_u64.to_ne_bytes();
            // The queue owns a nonblocking eventfd. A full counter is already
            // readable, so notification never waits for the input thread.
            while let Err(rustix::io::Errno::INTR) = rustix::io::write(fd.as_ref(), &increment) {}
        }
    }
}

struct FinishedGuard {
    finished: Arc<AtomicBool>,
    completion: CompletionWake,
    sender: Option<crossbeam_channel::Sender<DeleteResult>>,
}

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        // On unwind, the receiver must already be disconnected when it wakes.
        // Otherwise it could observe Empty and consume the only notification.
        self.sender.take();
        self.finished.store(true, Ordering::Release);
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

/// The queue's nonblocking eventfd wakes input polling after final publication.
#[cfg(target_os = "linux")]
pub(crate) fn spawn_with_completion(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: Option<ConfirmedTarget>,
    completion: Arc<rustix::fd::OwnedFd>,
) -> DeleteHandle {
    spawn_inner(
        anchor,
        relative,
        target,
        CompletionWake {
            fd: Some(completion),
        },
    )
}

fn spawn_inner(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: Option<ConfirmedTarget>,
    completion: CompletionWake,
) -> DeleteHandle {
    let (sender, receiver) = bounded(1);
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
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
                finished: worker_finished,
                completion: worker_completion,
                sender: Some(sender),
            };
            let mut removal = remove_expected(&anchor, &relative, &worker_state, target)
                .unwrap_or_else(Removal::failed);
            if let Err(error) = &removal.outcome {
                if error.kind() != io::ErrorKind::Interrupted && removal.errors.is_empty() {
                    removal.errors.push(DeleteError {
                        path: worker_path,
                        message: error.to_string(),
                    });
                }
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
                    progress: worker_state.snapshot(),
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
        finished.store(true, Ordering::Release);
        completion.signal();
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
    use std::os::fd::AsRawFd;

    let parent = match anchor.open_parent_cancellable(relative, &state.cancel) {
        Ok(parent) => parent,
        // Another queued deletion may have removed this target's parent.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Removal::already_absent());
        }
        Err(error) => return Err(error),
    };
    state.check_cancel()?;
    let parent_path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let held = retry_operation(state, || rustix::fs::fstat(&parent))?;
    if expected.is_some_and(|expected| expected.parent != Some(FileIdentity::from_stat(&held))) {
        return Err(changed_target());
    }
    let visible = retry_operation(state, || rustix::fs::stat(&parent_path))?;
    if rustix::fs::FileType::from_raw_mode(visible.st_mode) != rustix::fs::FileType::Directory
        || visible.st_dev != held.st_dev
        || visible.st_ino != held.st_ino
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
    let mut errors = Vec::new();
    let mut target_removed = false;
    let mut path = std::path::absolute(anchor.root_path())?.join(relative);
    let mut outcome = remove_from_parent(
        &parent,
        basename,
        state,
        expected,
        &mut path,
        &mut errors,
        &mut target_removed,
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
    errors_at_entry: usize,
}

#[cfg(target_os = "linux")]
struct DirectoryName {
    name: std::ffi::OsString,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn descriptor_path(fd: &rustix::fd::OwnedFd) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// O_PATH descriptors cannot use fchmod. The proc path names the held inode,
/// never a user-controlled pathname; symlink inodes are explicitly rejected.
#[cfg(target_os = "linux")]
fn chmod_held(
    fd: &rustix::fd::OwnedFd,
    mode: rustix::fs::Mode,
    state: &WorkerState,
) -> io::Result<()> {
    let held = retry_operation(state, || rustix::fs::fstat(fd))?;
    if rustix::fs::FileType::from_raw_mode(held.st_mode) == rustix::fs::FileType::Symlink {
        return Err(io::Error::other(
            "cannot change permissions through a symlink",
        ));
    }
    let path = descriptor_path(fd);
    let visible = retry_operation(state, || rustix::fs::stat(&path))?;
    if !identity_matches(&held, &visible) {
        return Err(changed_target());
    }
    retry_operation(state, || rustix::fs::chmod(&path, mode))
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn verify_held_mode(
    fd: &rustix::fd::OwnedFd,
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

#[cfg(target_os = "linux")]
fn repair_directory_permissions(
    fd: &rustix::fd::OwnedFd,
    state: &WorkerState,
    allow_ownership: bool,
) -> io::Result<Option<rustix::fs::Mode>> {
    use rustix::fs::{AtFlags, FileType, Mode, chownat, fstat};
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
        retry_operation(state, || {
            chownat(
                fd,
                "",
                Some(rustix::process::geteuid()),
                None,
                AtFlags::EMPTY_PATH,
            )
        })
        .map_err(|owner_error| {
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

#[cfg(target_os = "linux")]
fn checked_path_handle(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected: Option<&rustix::fs::Stat>,
    inode: u64,
    state: &WorkerState,
) -> io::Result<(rustix::fd::OwnedFd, rustix::fs::Stat)> {
    use rustix::fs::{Mode, OFlags, fstat, openat};
    let fd = retry_operation(state, || {
        openat(
            parent,
            name,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
    })?;
    let captured = retry_operation(state, || fstat(&fd))?;
    if expected.is_some_and(|expected| !identity_matches(expected, &captured))
        || (inode != 0 && inode != captured.st_ino)
    {
        return Err(changed_target());
    }
    Ok((fd, captured))
}

#[cfg(target_os = "linux")]
fn open_checked_directory(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected: Option<&rustix::fs::Stat>,
    inode: u64,
    state: &WorkerState,
) -> io::Result<Option<DirectoryFrame>> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = match retry_operation(state, || openat(parent, name, flags, Mode::empty())) {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let (held, captured) = match checked_path_handle(parent, name, expected, inode, state) {
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied && expected.is_none() =>
                {
                    // An enumerated directory can be readable but not searchable.
                    // Its retained parent is inside the confirmed subtree.
                    repair_directory_permissions(parent, state, true)?;
                    checked_path_handle(parent, name, expected, inode, state)?
                }
                result => result?,
            };
            if FileType::from_raw_mode(captured.st_mode) != FileType::Directory {
                return Err(changed_target());
            }
            if !checked_current(parent, name, &captured, state)? {
                return Ok(None);
            }
            repair_directory_permissions(&held, state, true)?;
            // Reopen the held directory itself, never resolve the entry again.
            retry_operation(state, || openat(&held, ".", flags, Mode::empty()))?
        }
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
        errors_at_entry: 0,
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
    #[cfg(test)]
    {
        use std::os::unix::ffi::OsStrExt;
        if state.deny_next_unlink.swap(false, Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let name = name.as_cow_c_str()?;
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

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn retry_failed_unlink(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    directory: bool,
    captured: Option<&rustix::fs::Stat>,
    inode: u64,
    restore_parent: bool,
    state: &WorkerState,
    original_error: io::Error,
    target_removed: Option<&mut bool>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, chownat};
    if original_error.kind() != io::ErrorKind::PermissionDenied {
        return Err(original_error);
    }
    let (target, captured) = match checked_path_handle(parent, name, captured, inode, state) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(target_removed) = target_removed {
                *target_removed = true;
            }
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !restore_parent => {
            repair_directory_permissions(parent, state, true)?;
            checked_path_handle(parent, name, captured, inode, state)?
        }
        result => result?,
    };
    if (FileType::from_raw_mode(captured.st_mode) == FileType::Directory) != directory {
        return Err(changed_target());
    }
    if !checked_current(parent, name, &captured, state)? {
        if let Some(target_removed) = target_removed {
            *target_removed = true;
        }
        return Ok(());
    }
    let repaired = repair_directory_permissions(parent, state, !restore_parent);
    // Sticky directories may additionally require ownership of the selected
    // inode. EMPTY_PATH acts on the retained inode and never follows a link.
    let ownership_error = if FileType::from_raw_mode(captured.st_mode) != FileType::Symlink
        && captured.st_uid != rustix::process::geteuid().as_raw()
    {
        retry_operation(state, || {
            chownat(
                &target,
                "",
                Some(rustix::process::geteuid()),
                None,
                AtFlags::EMPTY_PATH,
            )
        })
        .err()
    } else {
        None
    };
    let retry = checked_current(parent, name, &captured, state).and_then(|exists| {
        if exists {
            unlink_entry(parent, name, directory, state)
        } else {
            Ok(())
        }
    });
    // Record the unlink boundary before restoring an external parent's mode.
    // Restoration diagnostics must not resurrect a successfully removed target.
    if retry.is_ok() {
        if let Some(target_removed) = target_removed {
            *target_removed = true;
        }
    }
    // The target's containing directory can lie outside the requested subtree.
    // Restore its permissions even after cooperative cancellation.
    if restore_parent {
        if let Ok(Some(original_mode)) = repaired.as_ref() {
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
                let path = std::fs::read_link(descriptor_path(parent))
                    .unwrap_or_else(|_| descriptor_path(parent));
                io::Error::new(
                    error.kind(),
                    format!(
                        "could not restore permissions of {}: {error}",
                        path.display()
                    ),
                )
            })?;
        }
    }
    match retry {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
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

#[cfg(target_os = "linux")]
fn record_error(errors: &mut Vec<DeleteError>, path: &Path, error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::Interrupted {
        return Err(error);
    }
    errors.push(DeleteError {
        path: path.to_path_buf(),
        message: error.to_string(),
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_from_parent(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
    state: &WorkerState,
    expected: Option<ConfirmedTarget>,
    path: &mut PathBuf,
    errors: &mut Vec<DeleteError>,
    target_removed: &mut bool,
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
    state.examined_entries.fetch_add(1, Ordering::Relaxed);
    if expected.is_some_and(|expected| expected.target != Some(FileIdentity::from_stat(&captured)))
    {
        return Err(changed_target());
    }
    if !checked_current(parent, name, &captured, state)? {
        *target_removed = true;
        return Ok(());
    }
    if FileType::from_raw_mode(captured.st_mode) != FileType::Directory {
        return match unlink_entry(parent, name, false, state) {
            Ok(()) => {
                *target_removed = true;
                Ok(())
            }
            Err(error) => retry_failed_unlink(
                parent,
                name,
                false,
                Some(&captured),
                0,
                true,
                state,
                error,
                Some(target_removed),
            ),
        };
    }
    let Some(root) = open_checked_directory(parent, name, Some(&captured), 0, state)? else {
        *target_removed = true;
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
            path.push(&next.name);
            match open_checked_directory(&frame.fd, &next.name, None, next.inode, state) {
                Ok(Some(mut child)) => {
                    child.errors_at_entry = errors.len();
                    stack.push(child);
                    continue;
                }
                Ok(None) => {}
                Err(error) => record_error(errors, path, error)?,
            }
            path.pop();
            continue;
        }
        let frame = stack.last_mut().unwrap();
        if !frame.exhausted {
            match read_directory_batch(&frame.fd, &mut buffer, state, path, errors) {
                Ok((pending, exhausted)) => {
                    frame.pending = pending;
                    frame.exhausted = exhausted;
                }
                Err(error) => {
                    record_error(errors, path, error)?;
                    frame.exhausted = true;
                }
            }
            continue;
        }
        let finished = stack.pop().unwrap();
        let is_target = stack.is_empty();
        let containing_parent = stack.last().map(|frame| &frame.fd).unwrap_or(parent);
        let result = checked_current(containing_parent, &finished.name, &finished.captured, state)
            .and_then(|exists| {
                if !exists {
                    if is_target {
                        *target_removed = true;
                    }
                    return Ok(());
                }
                match unlink_entry(containing_parent, &finished.name, true, state) {
                    Ok(()) => {
                        if is_target {
                            *target_removed = true;
                        }
                        Ok(())
                    }
                    Err(error) => retry_failed_unlink(
                        containing_parent,
                        &finished.name,
                        true,
                        Some(&finished.captured),
                        0,
                        is_target,
                        state,
                        error,
                        if is_target {
                            Some(&mut *target_removed)
                        } else {
                            None
                        },
                    ),
                }
            });
        if let Err(error) = result {
            // A retained descendant already explains why its ancestors remain.
            if !(errors.len() > finished.errors_at_entry
                && error.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()))
            {
                record_error(errors, path, error)?;
            }
        }
        if !stack.is_empty() {
            path.pop();
        }
    }
    Ok(())
}

/// Consume one whole getdents buffer before suspending its descriptor. Files
/// retain the direct unlink fast path; errors alone allocate full paths.
#[cfg(target_os = "linux")]
fn read_directory_batch(
    fd: &rustix::fd::OwnedFd,
    buffer: &mut Vec<u8>,
    state: &WorkerState,
    path: &Path,
    errors: &mut Vec<DeleteError>,
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
                    Err(error) => {
                        record_error(errors, path, error.into())?;
                        return Ok((pending, true));
                    }
                };
                let bytes = entry.file_name().to_bytes();
                if bytes != b"." && bytes != b".." {
                    state.examined_entries.fetch_add(1, Ordering::Relaxed);
                    let kind = entry.file_type();
                    let name = std::ffi::OsStr::from_bytes(bytes);
                    let mut is_directory = kind == FileType::Directory;
                    if !is_directory {
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
                                        captured = Some(stat);
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                        continue;
                                    }
                                    Err(error) => {
                                        record_error(errors, &path.join(name), error)?;
                                        continue;
                                    }
                                }
                            }
                            if !is_directory {
                                if let Err(error) = retry_failed_unlink(
                                    fd,
                                    name,
                                    false,
                                    captured.as_ref(),
                                    entry.ino(),
                                    false,
                                    state,
                                    error,
                                    None,
                                ) {
                                    record_error(errors, &path.join(name), error)?;
                                }
                            }
                        }
                    }
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
    fn completion_notification_follows_result_and_finished_publication() {
        use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
        let work = fixture();
        std::fs::write(work.path().join("target"), b"remove").unwrap();
        let anchor = Arc::new(RootAnchor::open(work.path()).unwrap());
        let target = capture(&anchor, Path::new("target")).unwrap();
        let completion =
            Arc::new(eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap());
        let handle = spawn_with_completion(
            anchor,
            PathBuf::from("target"),
            Some(target),
            Arc::clone(&completion),
        );
        let mut fds = [PollFd::new(completion.as_ref(), PollFlags::IN)];
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
        assert_eq!(
            rustix::io::read(completion.as_ref(), &mut count).unwrap(),
            8
        );
        assert_eq!(u64::from_ne_bytes(count), 1);
        assert_eq!(
            rustix::io::read(completion.as_ref(), &mut count),
            Err(rustix::io::Errno::AGAIN)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_guard_wakes_on_unwind_and_never_waits_on_a_full_counter() {
        use rustix::event::{EventfdFlags, eventfd};
        let completion =
            Arc::new(eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap());
        let finished = Arc::new(AtomicBool::new(false));
        let wake = CompletionWake {
            fd: Some(Arc::clone(&completion)),
        };
        let (sender, receiver) = bounded(1);
        let result = std::panic::catch_unwind(|| {
            let _guard = FinishedGuard {
                finished: Arc::clone(&finished),
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
        assert!(finished.load(Ordering::Acquire));
        let mut count = [0_u8; 8];
        assert_eq!(
            rustix::io::read(completion.as_ref(), &mut count).unwrap(),
            8
        );
        assert_eq!(u64::from_ne_bytes(count), 1);
        rustix::io::write(completion.as_ref(), &(u64::MAX - 1).to_ne_bytes()).unwrap();
        finished.store(false, Ordering::Relaxed);
        drop(FinishedGuard {
            finished: Arc::clone(&finished),
            completion: wake,
            sender: None,
        });
        assert!(finished.load(Ordering::Acquire));
        assert_eq!(
            rustix::io::read(completion.as_ref(), &mut count).unwrap(),
            8
        );
        assert_eq!(u64::from_ne_bytes(count), u64::MAX - 1);
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
    fn failures_are_collected_with_full_paths_while_siblings_continue() {
        let work = fixture();
        std::fs::create_dir_all(work.path().join("target/nested/deeper")).unwrap();
        std::fs::create_dir(work.path().join("target/removable")).unwrap();
        let state = WorkerState::default();
        let mut expected_paths = std::collections::HashSet::new();
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn restored_mode_verification_detects_silent_special_bit_loss() {
        use rustix::fs::Mode;
        let work = fixture();
        let anchor = RootAnchor::open(work.path()).unwrap();
        let fd = anchor.open_directory(Path::new("")).unwrap();
        let state = WorkerState::default();
        // A successful chmod alone does not establish the requested final mode.
        let actual = Mode::from_raw_mode(0o500);
        chmod_held(&fd, actual, &state).unwrap();
        verify_held_mode(&fd, actual, &state).unwrap();
        for expected in [0o2500, 0o4500, 0o1500, 0o700] {
            let error = verify_held_mode(&fd, Mode::from_raw_mode(expected), &state).unwrap_err();
            assert!(error.to_string().contains("permission restoration"));
            assert!(error.to_string().contains(&format!("{expected:04o}")));
            assert!(error.to_string().contains("0500"));
        }
        chmod_held(&fd, Mode::RWXU, &state).unwrap();
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_directories_are_repaired_and_external_parent_mode_is_restored() {
        use std::os::unix::fs::PermissionsExt;
        let work = fixture();
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
        assert!(removal.outcome.is_ok(), "{:?}", removal.errors);
        assert!(removal.errors.is_empty());
        assert_eq!(mode, 0o500);
        assert!(!work.path().join("target").exists());
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
        assert!(chmod_held(&link, Mode::RWXU, &WorkerState::default()).is_err());
        let captured = statat(&parent, "target", AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::rename(work.path().join("target"), work.path().join("original")).unwrap();
        std::fs::write(work.path().join("target"), b"replacement").unwrap();
        let result = retry_failed_unlink(
            &parent,
            std::ffi::OsStr::new("target"),
            false,
            Some(&captured),
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
