//! Confirmed deletion runs outside the UI and remains rooted at an open directory.

use crate::backend::RootAnchor;
use crate::types::Sizes;
use crossbeam_channel::{Receiver, bounded};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

pub struct ParentMetadata {
    /// Empty means the scan root; otherwise relative to its retained anchor.
    pub relative: PathBuf,
    pub sizes: Sizes,
}

pub struct DeleteResult {
    pub relative: PathBuf,
    pub outcome: io::Result<()>,
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
}

pub struct DeleteHandle {
    pub receiver: Receiver<DeleteResult>,
    pub finished: Arc<AtomicBool>,
}

impl DeleteHandle {
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
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
    let (sender, receiver) = bounded(1);
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    let failure_sender = sender.clone();
    let failure_relative = relative.clone();
    let result = thread::Builder::new()
        .name("ldu-delete".into())
        .spawn(move || {
            let _finished = FinishedGuard(worker_finished);
            let removal = remove_confirmed(&anchor, &relative).unwrap_or_else(Removal::failed);
            let _ = sender.try_send(DeleteResult {
                relative,
                outcome: removal.outcome,
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
            parent_metadata: None,
            metadata_error: None,
        });
        finished.store(true, Ordering::Release);
    }
    DeleteHandle { receiver, finished }
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

fn remove_confirmed(anchor: &RootAnchor, relative: &Path) -> io::Result<Removal> {
    let relative = validated_relative(relative)?;
    #[cfg(target_os = "linux")]
    {
        remove_linux(anchor, &relative)
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

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Linux targets.
fn remove_linux(anchor: &RootAnchor, relative: &Path) -> io::Result<Removal> {
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let parent = anchor.open_parent(relative)?;
    let parent_path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let held = rustix::fs::fstat(&parent).map_err(io::Error::from)?;
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
    let target = parent_path.join(basename);
    let captured = fs::symlink_metadata(&target)?;
    let current = fs::symlink_metadata(&target)?;
    if captured.dev() != current.dev()
        || captured.ino() != current.ino()
        || captured.file_type() != current.file_type()
    {
        return Err(io::Error::other(
            "deletion target changed before removal; refresh and confirm again",
        ));
    }

    // The parent FD stays alive. std's Linux recursive removal does not follow symlinks.
    // Identity checking is best effort: replacement can still race the final removal.
    let outcome = if captured.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    };
    // Read the retained parent descriptor after the attempt, including partial failure.
    // A completed unlink stays successful if this metadata read fails.
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
}
