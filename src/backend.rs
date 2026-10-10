//! Directory enumeration and no-follow metadata, with a buffered Linux backend.

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::types::{NodeKind, Sizes};

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Backend {
    #[default]
    Auto,
    Native,
    Std,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryMetadata {
    pub kind: NodeKind,
    pub sizes: Sizes,
    pub identity: Option<(u64, u64)>,
    pub links: u64,
}

/// Shared by metadata jobs; native lookups remain relative to the open directory.
#[derive(Debug)]
pub enum MetadataReader {
    #[cfg(target_os = "linux")]
    Native(rustix::fd::OwnedFd),
    #[cfg(not(target_os = "linux"))]
    Std(PathBuf),
}

impl MetadataReader {
    pub fn metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Native(fd) => {
                use rustix::fs::{AtFlags, statat};
                loop {
                    match statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(stat) => return Ok(native_metadata(stat)),
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            Self::Std(path) => std_path_metadata(&path.join(name)),
        }
    }
}

/// A scan's root object. Linux operations resolve through its descriptor, never
/// through the display path after opening it in the scanner thread.
#[derive(Debug)]
pub struct RootAnchor {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    fd: rustix::fd::OwnedFd,
    #[cfg(target_os = "linux")]
    openat2_available: AtomicBool,
}

impl RootAnchor {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_cancellable(path, &AtomicBool::new(false))
    }

    /// Open the original root while checking cancellation between path components
    /// and interrupted retries. The resulting anchor has the same path guarantees.
    pub fn open_cancellable(path: &Path, cancel: &AtomicBool) -> io::Result<Self> {
        check_cancel(cancel)?;
        let path = normalize_root_path(path);
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty scan root",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{Mode, OFlags, open};
            let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let base_name = if path.is_absolute() { "/" } else { "." };
            let base = loop {
                check_cancel(cancel)?;
                match open(base_name, flags, Mode::empty()) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => break result.map_err(io::Error::from)?,
                }
            };
            let relative = if path.is_absolute() {
                path.strip_prefix("/").map_err(io::Error::other)?
            } else {
                path.as_path()
            };
            let fd = component_open_cancellable(
                &base,
                relative,
                OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                cancel,
            )?;
            check_cancel(cancel)?;
            Ok(Self {
                path,
                fd,
                openat2_available: AtomicBool::new(true),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            loop {
                check_cancel(cancel)?;
                match fs::symlink_metadata(&path) {
                    Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            check_cancel(cancel)?;
            Ok(Self { path })
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> io::Result<EntryMetadata> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match rustix::fs::fstat(&self.fd) {
                    Ok(stat) => return Ok(native_metadata(stat)),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            std_path_metadata(&self.path)
        }
    }

    #[cfg(target_os = "linux")]
    pub fn open_directory(&self, relative: &Path) -> io::Result<rustix::fd::OwnedFd> {
        self.open_directory_cancellable(relative, &AtomicBool::new(false))
    }

    /// Preserve anchored resolution while allowing cancellation between opens
    /// and interrupted retries, including the deep-path component fallback.
    #[cfg(target_os = "linux")]
    pub fn open_directory_cancellable(
        &self,
        relative: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<rustix::fd::OwnedFd> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
        check_open_cancel(cancel)?;
        validate_relative(relative)?;
        let path = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        if self.openat2_available.load(Ordering::Relaxed) {
            loop {
                check_open_cancel(cancel)?;
                match openat2(
                    &self.fd,
                    path,
                    flags,
                    Mode::empty(),
                    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
                ) {
                    Ok(fd) => {
                        check_open_cancel(cancel)?;
                        return Ok(fd);
                    }
                    Err(rustix::io::Errno::INTR) => continue,
                    // Whole paths can exceed PATH_MAX while each component is valid.
                    Err(rustix::io::Errno::NAMETOOLONG) => break,
                    Err(
                        rustix::io::Errno::NOSYS
                        | rustix::io::Errno::INVAL
                        | rustix::io::Errno::PERM,
                    ) => {
                        self.openat2_available.store(false, Ordering::Relaxed);
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        component_open_cancellable(&self.fd, relative, flags, cancel)
    }

    /// Open a selected entry's parent while retaining the original scan root.
    #[cfg(target_os = "linux")]
    pub fn open_parent(&self, relative: &Path) -> io::Result<rustix::fd::OwnedFd> {
        self.open_parent_cancellable(relative, &AtomicBool::new(false))
    }

    #[cfg(target_os = "linux")]
    pub fn open_parent_cancellable(
        &self,
        relative: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<rustix::fd::OwnedFd> {
        check_open_cancel(cancel)?;
        validate_relative(relative)?;
        if relative.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the scan root has no selectable parent",
            ));
        }
        self.open_directory_cancellable(relative.parent().unwrap_or(Path::new("")), cancel)
    }
}

/// Components drop redundant separators and terminal `.` without resolving a
/// symlink. Keeping `..` preserves the user's selected path's semantics.
pub fn normalize_root_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn validate_relative(relative: &Path) -> io::Result<()> {
    use std::path::Component;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "operation path must stay beneath the scan root",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn component_open_cancellable(
    base: &rustix::fd::OwnedFd,
    path: &Path,
    final_flags: rustix::fs::OFlags,
    cancel: &AtomicBool,
) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::OFlags;
    use std::path::Component;
    let mut components = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .peekable();
    let mut current = None;
    while let Some(component) = components.next() {
        let name = match component {
            Component::Normal(name) => name,
            Component::ParentDir => OsStr::new(".."),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid anchored path",
                ));
            }
        };
        let flags = if components.peek().is_none() {
            final_flags
        } else {
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
        };
        let next =
            open_component_cancellable(current.as_ref().unwrap_or(base), name, flags, cancel)?;
        current = Some(next);
    }
    match current {
        Some(fd) => Ok(fd),
        None => open_component_cancellable(base, OsStr::new("."), final_flags, cancel),
    }
}

#[cfg(target_os = "linux")]
fn open_component_cancellable(
    parent: &rustix::fd::OwnedFd,
    name: &OsStr,
    flags: rustix::fs::OFlags,
    cancel: &AtomicBool,
) -> io::Result<rustix::fd::OwnedFd> {
    loop {
        check_open_cancel(cancel)?;
        match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
            Err(rustix::io::Errno::INTR) => continue,
            result => {
                let fd = result.map_err(io::Error::from)?;
                check_open_cancel(cancel)?;
                return Ok(fd);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn check_open_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "anchored operation cancelled",
        ))
    } else {
        Ok(())
    }
}

pub fn root_metadata(path: &Path) -> io::Result<EntryMetadata> {
    #[cfg(target_os = "linux")]
    {
        RootAnchor::open(path)?.metadata()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std_path_metadata(&normalize_root_path(path))
    }
}

#[cfg(not(target_os = "linux"))]
fn std_path_metadata(path: &Path) -> io::Result<EntryMetadata> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => return Ok(std_metadata(metadata)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Standalone enumeration also anchors its final component before reading it.
pub fn enumerate(
    path: &Path,
    backend: Backend,
    buffer_size: usize,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: impl FnMut(Vec<OsString>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let anchor = RootAnchor::open_cancellable(path, cancel)?;
    enumerate_anchored(
        &anchor,
        Path::new(""),
        backend,
        buffer_size,
        batch_size,
        cancel,
        emit,
    )
}

/// Scanner tasks carry root-relative paths. Captured descriptors protect both
/// enumeration and metadata even if a root or ancestor pathname is replaced.
pub fn enumerate_anchored(
    anchor: &RootAnchor,
    relative: &Path,
    backend: Backend,
    buffer_size: usize,
    batch_size: usize,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<OsString>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let batch_size = batch_size.max(1);
    #[cfg(target_os = "linux")]
    {
        let reader = MetadataReader::Native(anchor.open_directory_cancellable(relative, cancel)?);
        if backend != Backend::Std {
            let mut emitted = false;
            let result = enumerate_native(
                &reader,
                buffer_size,
                batch_size,
                cancel,
                &mut |names, reader| {
                    emitted = true;
                    emit(names, reader)
                },
            );
            match result {
                Err(error) if backend == Backend::Auto && !emitted && unsupported(&error) => {}
                result => return result,
            }
        }
        let MetadataReader::Native(fd) = &reader;
        use std::os::fd::AsRawFd;
        let path = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        enumerate_std(&path, &reader, batch_size, cancel, &mut emit)
    }
    #[cfg(not(target_os = "linux"))]
    {
        validate_relative(relative)?;
        let _ = buffer_size;
        if backend == Backend::Native {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native directory enumeration requires Linux",
            ));
        }
        let path = normalize_root_path(&anchor.root_path().join(relative));
        if std_path_metadata(&path)?.kind != NodeKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "enumeration requires a directory without a symbolic link",
            ));
        }
        let reader = MetadataReader::Std(path);
        let MetadataReader::Std(path) = &reader;
        enumerate_std(path, &reader, batch_size, cancel, &mut emit)
    }
}

fn enumerate_std(
    path: &Path,
    reader: &MetadataReader,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Vec<OsString>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    let entries = fs::read_dir(path)?;
    let mut batch = Vec::new();
    for entry in entries {
        check_cancel(cancel)?;
        match entry {
            Ok(entry) => {
                if batch.is_empty() {
                    batch.reserve_exact(batch_size);
                }
                batch.push(entry.file_name());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                flush(&mut batch, reader, emit)?;
                return Err(error);
            }
        }
        if batch.len() == batch_size {
            flush(&mut batch, reader, emit)?;
        }
    }
    check_cancel(cancel)?;
    flush(&mut batch, reader, emit)
}

#[cfg(target_os = "linux")]
fn enumerate_native(
    reader: &MetadataReader,
    buffer_size: usize,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Vec<OsString>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    use rustix::fs::RawDir;
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;

    let MetadataReader::Native(fd) = reader;
    let mut buffer = Vec::<u8>::new();
    buffer
        .try_reserve_exact(buffer_size.max(4096))
        .map_err(io::Error::other)?;
    let mut batch = Vec::new();
    loop {
        let mut resize = false;
        let capacity = buffer.capacity();
        {
            let mut entries = RawDir::new(fd, buffer.spare_capacity_mut());
            loop {
                check_cancel(cancel)?;
                let Some(entry) = entries.next() else {
                    break;
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(Errno::INTR) => continue,
                    Err(Errno::INVAL) if capacity < 1024 * 1024 => {
                        resize = true;
                        break;
                    }
                    Err(error) => {
                        flush(&mut batch, reader, emit)?;
                        return Err(error.into());
                    }
                };
                let bytes = entry.file_name().to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                if batch.is_empty() {
                    batch.reserve_exact(batch_size);
                }
                batch.push(OsStr::from_bytes(bytes).to_owned());
                if batch.len() == batch_size {
                    flush(&mut batch, reader, emit)?;
                }
            }
        }
        if !resize {
            break;
        }
        let new_capacity = buffer.capacity().saturating_mul(2).min(1024 * 1024);
        buffer
            .try_reserve_exact(new_capacity)
            .map_err(io::Error::other)?;
    }
    check_cancel(cancel)?;
    flush(&mut batch, reader, emit)
}

fn flush(
    batch: &mut Vec<OsString>,
    reader: &MetadataReader,
    emit: &mut impl FnMut(Vec<OsString>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    emit(std::mem::take(batch), reader)
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || matches!(
            error.raw_os_error(),
            Some(code) if code == rustix::io::Errno::NOSYS.raw_os_error()
                || code == rustix::io::Errno::OPNOTSUPP.raw_os_error()
        )
}

#[cfg(not(target_os = "linux"))]
fn std_metadata(metadata: fs::Metadata) -> EntryMetadata {
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_file() {
        NodeKind::File
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    };
    #[cfg(unix)]
    let (allocated, identity, links) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.blocks().saturating_mul(512),
            Some((metadata.dev(), metadata.ino())),
            metadata.nlink(),
        )
    };
    // std does not expose allocated blocks on non-Unix targets.
    #[cfg(not(unix))]
    let (allocated, identity, links) = (metadata.len(), None, 1);
    EntryMetadata {
        kind,
        sizes: Sizes {
            apparent: metadata.len(),
            allocated,
        },
        identity,
        links,
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Linux targets.
fn native_metadata(stat: rustix::fs::Stat) -> EntryMetadata {
    use rustix::fs::FileType;
    let kind = match FileType::from_raw_mode(stat.st_mode) {
        FileType::Directory => NodeKind::Directory,
        FileType::RegularFile => NodeKind::File,
        FileType::Symlink => NodeKind::Symlink,
        _ => NodeKind::Other,
    };
    EntryMetadata {
        kind,
        sizes: Sizes {
            apparent: u64::try_from(stat.st_size).unwrap_or(0),
            allocated: u64::try_from(stat.st_blocks)
                .unwrap_or(0)
                .saturating_mul(512),
        },
        identity: Some((stat.st_dev as u64, stat.st_ino as u64)),
        links: stat.st_nlink as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn fixture() -> tempfile::TempDir {
        fs::create_dir_all(".tmp").unwrap();
        tempfile::Builder::new()
            .prefix("backend-")
            .tempdir_in(".tmp")
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_and_std_agree_on_raw_names_sparse_files_and_symlinks() {
        use std::collections::HashMap;
        use std::os::unix::{ffi::OsStringExt, fs::symlink};

        fn assert_sync<T: Sync>() {}
        assert_sync::<MetadataReader>();
        let directory = fixture();
        let raw_name = OsString::from_vec(vec![b'f', 0xff]);
        fs::write(directory.path().join(&raw_name), b"contents").unwrap();
        fs::create_dir(directory.path().join("child")).unwrap();
        fs::File::create(directory.path().join("sparse"))
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        symlink(&raw_name, directory.path().join("link")).unwrap();
        let cancelled = AtomicBool::new(false);
        let read = |backend| {
            let mut entries = HashMap::new();
            enumerate(
                directory.path(),
                backend,
                4096,
                2,
                &cancelled,
                |names, reader| {
                    assert!(names.len() <= 2);
                    for name in names {
                        let metadata = reader.metadata(&name)?;
                        assert!(entries.insert(name, metadata).is_none());
                    }
                    Ok(())
                },
            )
            .unwrap();
            entries
        };
        let native = read(Backend::Native);
        let standard = read(Backend::Std);
        assert_eq!(native.len(), 4);
        assert!(native.contains_key(&raw_name));
        for (name, expected) in standard {
            let actual = &native[&name];
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.sizes.apparent, expected.sizes.apparent);
            assert_eq!(actual.sizes.allocated, expected.sizes.allocated);
            assert_eq!(actual.identity, expected.identity);
            assert_eq!(actual.links, expected.links);
        }
        assert_eq!(native[OsStr::new("link")].kind, NodeKind::Symlink);
        assert_eq!(
            native[OsStr::new("sparse")].sizes.apparent,
            16 * 1024 * 1024
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_symlink_is_rejected_without_fallback_or_entries() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir(directory.path().join("real")).unwrap();
        symlink("real", directory.path().join("alias")).unwrap();
        let cancel = AtomicBool::new(false);
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            assert!(
                enumerate(
                    &directory.path().join("alias"),
                    backend,
                    4096,
                    2,
                    &cancel,
                    |_, _| panic!("a directory symlink must not be traversed"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn pre_cancelled_enumeration_does_not_open_the_path() {
        let cancel = AtomicBool::new(true);
        for path in [Path::new("a-path-that-does-not-exist"), Path::new("")] {
            let error = RootAnchor::open_cancellable(path, &cancel).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
        let result = enumerate(
            Path::new("a-path-that-does-not-exist"),
            Backend::Auto,
            4096,
            2,
            &cancel,
            |_, _| panic!("cancelled enumeration must not emit"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn callback_error_does_not_trigger_duplicate_fallback() {
        let directory = fixture();
        fs::write(directory.path().join("file"), b"data").unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let result = enumerate(directory.path(), Backend::Auto, 4096, 1, &cancel, |_, _| {
            calls += 1;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "callback stopped",
            ))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(calls, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trailing_symlink_root_components_never_follow_the_target() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir(directory.path().join("real")).unwrap();
        fs::write(
            directory.path().join("real/outside-data"),
            b"must stay outside",
        )
        .unwrap();
        symlink("real", directory.path().join("alias")).unwrap();
        let cancel = AtomicBool::new(false);
        for suffix in ["alias/", "alias/.", "alias//./"] {
            let path = directory.path().join(suffix);
            assert_eq!(root_metadata(&path).unwrap().kind, NodeKind::Symlink);
            for backend in [Backend::Auto, Backend::Native, Backend::Std] {
                assert!(
                    enumerate(&path, backend, 4096, 2, &cancel, |_, _| panic!(
                        "a trailing separator must not bypass no-follow"
                    ))
                    .is_err()
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_root_survives_display_ancestor_replacement_for_all_backends() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir_all(directory.path().join("original/root")).unwrap();
        fs::create_dir_all(directory.path().join("outside/root")).unwrap();
        fs::write(directory.path().join("original/root/payload"), b"original").unwrap();
        fs::write(directory.path().join("outside/root/payload"), vec![0; 1024]).unwrap();
        let anchor = RootAnchor::open(&directory.path().join("original/root")).unwrap();
        let original_identity = anchor.metadata().unwrap().identity;
        fs::rename(
            directory.path().join("original"),
            directory.path().join("saved"),
        )
        .unwrap();
        symlink("outside", directory.path().join("original")).unwrap();
        let cancel = AtomicBool::new(false);
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            let mut count = 0;
            enumerate_anchored(
                &anchor,
                Path::new(""),
                backend,
                4096,
                1,
                &cancel,
                |names, reader| {
                    for name in names {
                        assert_eq!(name, "payload");
                        assert_eq!(reader.metadata(&name)?.sizes.apparent, 8);
                        count += 1;
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(count, 1);
            assert_eq!(anchor.metadata().unwrap().identity, original_identity);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descendant_symlink_replacement_is_rejected_in_fast_and_fallback_open() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir_all(directory.path().join("root/branch/nested")).unwrap();
        fs::create_dir_all(directory.path().join("outside/nested")).unwrap();
        let anchor = RootAnchor::open(&directory.path().join("root")).unwrap();
        fs::rename(
            directory.path().join("root/branch"),
            directory.path().join("root/saved"),
        )
        .unwrap();
        symlink("../outside", directory.path().join("root/branch")).unwrap();
        assert!(anchor.open_directory(Path::new("branch/nested")).is_err());
        anchor.openat2_available.store(false, Ordering::Relaxed);
        assert!(anchor.open_directory(Path::new("branch/nested")).is_err());
        assert!(anchor.open_directory(Path::new("../outside")).is_err());
        assert!(anchor.open_directory(Path::new("/outside")).is_err());
        assert!(anchor.open_parent(Path::new("")).is_err());
        assert!(anchor.open_parent(Path::new(".")).is_err());
        let cancel = AtomicBool::new(false);
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            assert!(
                enumerate_anchored(
                    &anchor,
                    Path::new("branch/nested"),
                    backend,
                    4096,
                    1,
                    &cancel,
                    |_, _| panic!("a replaced ancestor must not expose outside entries")
                )
                .is_err()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn long_relative_paths_use_component_fallback_without_disabling_fast_open() {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        use std::io::Write as _;
        use std::os::unix::ffi::OsStringExt;

        let directory = fixture();
        let anchor = RootAnchor::open(directory.path()).unwrap();
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let mut current = anchor.open_directory(Path::new("")).unwrap();
        let fast_open_available = anchor.openat2_available.load(Ordering::Relaxed);
        let component = "d".repeat(200);
        let mut relative = PathBuf::new();
        for _ in 0..22 {
            mkdirat(&current, component.as_str(), Mode::RWXU).unwrap();
            current = openat(&current, component.as_str(), flags, Mode::empty()).unwrap();
            relative.push(&component);
        }
        assert!(relative.as_os_str().as_encoded_bytes().len() > 4096);
        let raw_name = OsString::from_vec(b"payload-\xff".to_vec());
        let mut payload: fs::File = openat(
            &current,
            &raw_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap()
        .into();
        payload.write_all(b"payload").unwrap();
        let expected = native_metadata(rustix::fs::fstat(&payload).unwrap());
        drop(payload);

        let cancel = AtomicBool::new(false);
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            let mut count = 0;
            enumerate_anchored(
                &anchor,
                &relative,
                backend,
                4096,
                2,
                &cancel,
                |names, reader| {
                    for name in names {
                        assert_eq!(name, raw_name);
                        let metadata = reader.metadata(&name)?;
                        assert_eq!(metadata.kind, NodeKind::File);
                        assert_eq!(metadata.sizes, expected.sizes);
                        assert_eq!(metadata.identity, expected.identity);
                        assert_eq!(metadata.links, expected.links);
                        count += 1;
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(count, 1);
            assert_eq!(
                anchor.openat2_available.load(Ordering::Relaxed),
                fast_open_available,
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_anchored_opens_skip_fast_and_component_resolution() {
        std::fs::create_dir_all(".tmp").unwrap();
        let work = tempfile::Builder::new()
            .prefix("cancel-open-")
            .tempdir_in(".tmp")
            .unwrap();
        let cancel = AtomicBool::new(false);
        let anchor = RootAnchor::open_cancellable(work.path(), &cancel).unwrap();
        cancel.store(true, Ordering::Relaxed);
        let error = RootAnchor::open_cancellable(work.path(), &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        for fast in [true, false] {
            anchor.openat2_available.store(fast, Ordering::Relaxed);
            let error = anchor
                .open_directory_cancellable(Path::new("missing/child"), &cancel)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            let error = anchor
                .open_parent_cancellable(Path::new("missing/target"), &cancel)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            cancel.store(false, Ordering::Relaxed);
            assert!(
                anchor
                    .open_directory_cancellable(Path::new(""), &cancel)
                    .is_ok()
            );
            assert!(
                anchor
                    .open_parent_cancellable(Path::new("../outside"), &cancel)
                    .is_err()
            );
            cancel.store(true, Ordering::Relaxed);
        }
    }
}
