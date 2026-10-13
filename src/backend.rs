//! Anchored directory enumeration and no-follow metadata on Unix.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(any(target_os = "linux", not(unix), test))]
use std::fs;

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
    #[cfg(unix)]
    Native(rustix::fd::OwnedFd),
    #[cfg(not(unix))]
    Std(PathBuf),
}

impl MetadataReader {
    pub fn metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        match self {
            #[cfg(unix)]
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
            #[cfg(not(unix))]
            Self::Std(path) => std_path_metadata(&path.join(name)),
        }
    }
}

/// A scan's root object. Unix operations resolve through its descriptor, never
/// through the display path after opening it in the scanner thread.
#[derive(Debug)]
pub struct RootAnchor {
    path: PathBuf,
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
    // Platforms without O_PATH retain a snapshot for a non-directory root. Such
    // a root cannot be traversed, so its parent descriptor is never used as it.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "freebsd"))))]
    leaf: Option<EntryMetadata>,
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
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, open};
            let flags = search_directory_flags();
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
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            let fd = component_open_cancellable(
                &base,
                relative,
                rustix::fs::OFlags::PATH
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                cancel,
            )?;
            #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
            let (fd, leaf) = open_portable_root(&base, relative, cancel)?;
            check_cancel(cancel)?;
            Ok(Self {
                path,
                fd,
                #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
                leaf,
                #[cfg(target_os = "linux")]
                openat2_available: AtomicBool::new(true),
            })
        }
        #[cfg(not(unix))]
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
        #[cfg(unix)]
        {
            #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
            if let Some(metadata) = self.leaf {
                return Ok(metadata);
            }
            loop {
                match rustix::fs::fstat(&self.fd) {
                    Ok(stat) => return Ok(native_metadata(stat)),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        #[cfg(not(unix))]
        {
            std_path_metadata(&self.path)
        }
    }

    #[cfg(unix)]
    pub fn open_directory(&self, relative: &Path) -> io::Result<rustix::fd::OwnedFd> {
        self.open_directory_cancellable(relative, &AtomicBool::new(false))
    }

    /// Preserve anchored resolution while allowing cancellation between opens
    /// and interrupted retries, including the deep-path component fallback.
    #[cfg(unix)]
    pub fn open_directory_cancellable(
        &self,
        relative: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<rustix::fd::OwnedFd> {
        use rustix::fs::OFlags;
        self.open_directory_with_flags_cancellable(
            relative,
            cancel,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        )
    }

    #[cfg(unix)]
    fn open_directory_with_flags_cancellable(
        &self,
        relative: &Path,
        cancel: &AtomicBool,
        flags: rustix::fs::OFlags,
    ) -> io::Result<rustix::fd::OwnedFd> {
        check_open_cancel(cancel)?;
        validate_relative(relative)?;
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        if self.leaf.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "the scan root is not a directory",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{Mode, ResolveFlags, openat2};
            let path = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative
            };
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
        }
        component_open_cancellable(&self.fd, relative, flags, cancel)
    }

    /// Record the logical ancestors of a directory deletion without reopening
    /// the display path. The caller retains its parent descriptor throughout
    /// this walk; a changed parent fails rather than retargeting the operation.
    #[cfg(unix)]
    pub(crate) fn parent_ancestors_cancellable(
        &self,
        relative: &Path,
        expected_parent: (u64, u64),
        cancel: &AtomicBool,
    ) -> io::Result<std::collections::HashSet<(u64, u64)>> {
        check_open_cancel(cancel)?;
        validate_relative(relative)?;
        let Some(parent_path) = relative.parent().filter(|_| relative.file_name().is_some()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the scan root has no selectable parent",
            ));
        };
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        if self.leaf.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "the scan root is not a directory",
            ));
        }
        let mut ancestors = std::collections::HashSet::new();
        let mut identity = record_parent_ancestor(&self.fd, &mut ancestors, cancel)?;
        let mut current = None;
        for component in parent_path.components() {
            let std::path::Component::Normal(name) = component else {
                continue;
            };
            let next = open_component_cancellable(
                current.as_ref().unwrap_or(&self.fd),
                name,
                search_directory_flags(),
                cancel,
            )?;
            identity = record_parent_ancestor(&next, &mut ancestors, cancel)?;
            current = Some(next);
        }
        if identity != expected_parent {
            return Err(io::Error::other(
                "the deletion parent changed during ancestor checks",
            ));
        }
        check_open_cancel(cancel)?;
        Ok(ancestors)
    }

    /// Open a selected entry's parent while retaining the original scan root.
    #[cfg(unix)]
    pub fn open_parent(&self, relative: &Path) -> io::Result<rustix::fd::OwnedFd> {
        self.open_parent_cancellable(relative, &AtomicBool::new(false))
    }

    #[cfg(unix)]
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
        // Keep searchable-only descriptors where the OS supports them; requiring
        // read permission would reject otherwise deletable entries.
        self.open_directory_with_flags_cancellable(
            relative.parent().unwrap_or(Path::new("")),
            cancel,
            search_directory_flags(),
        )
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

/// Retain ancestors without reading them where supported. Darwin O_EVTONLY
/// still requires read permission unless its process I/O policy disables that
/// requirement; every platform preserves no-follow descriptor resolution.
#[cfg(unix)]
fn search_directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let access = OFlags::PATH;
    #[cfg(target_os = "macos")]
    let access = OFlags::from_bits_retain(libc::O_EVTONLY as _);
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "freebsd"))))]
fn open_portable_root(
    base: &rustix::fd::OwnedFd,
    relative: &Path,
    cancel: &AtomicBool,
) -> io::Result<(rustix::fd::OwnedFd, Option<EntryMetadata>)> {
    let Some(name) = relative.file_name() else {
        let fd = component_open_cancellable(base, relative, search_directory_flags(), cancel)?;
        return Ok((fd, None));
    };
    let parent = component_open_cancellable(
        base,
        relative.parent().unwrap_or(Path::new("")),
        search_directory_flags(),
        cancel,
    )?;
    let metadata = loop {
        check_open_cancel(cancel)?;
        match rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => break native_metadata(stat),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    };
    check_open_cancel(cancel)?;
    if metadata.kind == NodeKind::Directory {
        let fd = open_component_cancellable(&parent, name, search_directory_flags(), cancel)?;
        Ok((fd, None))
    } else {
        // Never open a FIFO, device, or symlink merely to retain root metadata.
        // The captured parent stays alive, but every traversal rejects this root.
        Ok((parent, Some(metadata)))
    }
}

#[cfg(unix)]
fn component_open_cancellable(
    base: &rustix::fd::OwnedFd,
    path: &Path,
    final_flags: rustix::fs::OFlags,
    cancel: &AtomicBool,
) -> io::Result<rustix::fd::OwnedFd> {
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
            search_directory_flags()
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

#[cfg(unix)]
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

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Unix targets.
fn record_parent_ancestor(
    fd: &rustix::fd::OwnedFd,
    ancestors: &mut std::collections::HashSet<(u64, u64)>,
    cancel: &AtomicBool,
) -> io::Result<(u64, u64)> {
    let stat = loop {
        check_open_cancel(cancel)?;
        match rustix::fs::fstat(fd) {
            Err(rustix::io::Errno::INTR) => continue,
            result => break result.map_err(io::Error::from)?,
        }
    };
    check_open_cancel(cancel)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "a deletion ancestor is not a directory",
        ));
    }
    let identity = (stat.st_dev as u64, stat.st_ino as u64);
    if !ancestors.insert(identity) {
        return Err(io::Error::other(
            "deletion parent path refers to an ancestor directory",
        ));
    }
    Ok(identity)
}

#[cfg(unix)]
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
    #[cfg(unix)]
    {
        RootAnchor::open(path)?.metadata()
    }
    #[cfg(not(unix))]
    {
        std_path_metadata(&normalize_root_path(path))
    }
}

#[cfg(not(unix))]
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
    emit: impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
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
    mut emit: impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
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
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = (backend, buffer_size);
        let reader = MetadataReader::Native(anchor.open_directory_cancellable(relative, cancel)?);
        enumerate_portable(&reader, batch_size, cancel, &mut emit)
    }
    #[cfg(not(unix))]
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

#[cfg(any(target_os = "linux", not(unix)))]
fn enumerate_std(
    path: &Path,
    reader: &MetadataReader,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
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
                batch.push(Arc::from(entry.file_name()));
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

/// fdopendir/readdir supplies the platform's buffered directory stream on BSD
/// and macOS. Keeping metadata on the original descriptor preserves anchoring.
#[cfg(all(unix, any(not(target_os = "linux"), test)))]
fn enumerate_portable(
    reader: &MetadataReader,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let MetadataReader::Native(fd) = reader;
    let mut entries = loop {
        check_cancel(cancel)?;
        match rustix::fs::Dir::read_from(fd) {
            Ok(entries) => break entries,
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let mut batch = Vec::new();
    loop {
        check_cancel(cancel)?;
        let Some(entry) = entries.read() else {
            break;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Dir permanently stops after any error, including EINTR. Do
                // not turn a partial listing into a successful empty tail.
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
        batch.push(Arc::from(OsStr::from_bytes(bytes)));
        if batch.len() == batch_size {
            flush(&mut batch, reader, emit)?;
        }
    }
    check_cancel(cancel)?;
    flush(&mut batch, reader, emit)
}

#[cfg(target_os = "linux")]
const NATIVE_BUFFER_LIMIT: usize = 1024 * 1024;

#[cfg(target_os = "linux")]
thread_local! {
    static NATIVE_READ_BUFFER: std::cell::RefCell<Vec<u8>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(target_os = "linux")]
struct NativeReadBuffer {
    bytes: Vec<u8>,
    window: usize,
}

#[cfg(target_os = "linux")]
impl NativeReadBuffer {
    fn new(window: usize) -> io::Result<Self> {
        // Own the buffer across callbacks, which can reenter scanning on this thread.
        let bytes = NATIVE_READ_BUFFER.with(|cache| std::mem::take(&mut *cache.borrow_mut()));
        let mut buffer = Self {
            bytes,
            window: window.max(4096),
        };
        buffer
            .bytes
            .try_reserve_exact(buffer.window)
            .map_err(io::Error::other)?;
        Ok(buffer)
    }

    fn spare_capacity_mut(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
        // Cached allocation capacity must not override the configured read size.
        &mut self.bytes.spare_capacity_mut()[..self.window]
    }

    fn grow(&mut self) -> io::Result<()> {
        self.window = self.window.saturating_mul(2).min(NATIVE_BUFFER_LIMIT);
        self.bytes
            .try_reserve_exact(self.window)
            .map_err(io::Error::other)
    }
}

#[cfg(target_os = "linux")]
impl Drop for NativeReadBuffer {
    fn drop(&mut self) {
        if self.bytes.capacity() > NATIVE_BUFFER_LIMIT {
            return;
        }
        let _ = NATIVE_READ_BUFFER.try_with(|cache| {
            let mut cached = cache.borrow_mut();
            if self.bytes.capacity() > cached.capacity() {
                std::mem::swap(&mut *cached, &mut self.bytes);
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn enumerate_native(
    reader: &MetadataReader,
    buffer_size: usize,
    batch_size: usize,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
) -> io::Result<()> {
    use rustix::fs::RawDir;
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;

    let MetadataReader::Native(fd) = reader;
    let mut buffer = NativeReadBuffer::new(buffer_size)?;
    let mut batch = Vec::new();
    loop {
        let mut resize = false;
        let window = buffer.window;
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
                    Err(Errno::INVAL) if window < NATIVE_BUFFER_LIMIT => {
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
                batch.push(Arc::from(OsStr::from_bytes(bytes)));
                if batch.len() == batch_size {
                    flush(&mut batch, reader, emit)?;
                }
            }
        }
        if !resize {
            break;
        }
        buffer.grow()?;
    }
    check_cancel(cancel)?;
    flush(&mut batch, reader, emit)
}

fn flush(
    batch: &mut Vec<Arc<OsStr>>,
    reader: &MetadataReader,
    emit: &mut impl FnMut(Vec<Arc<OsStr>>, &MetadataReader) -> io::Result<()>,
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

#[cfg(not(unix))]
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

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // Stat field widths differ between Unix targets.
pub(crate) fn native_metadata(stat: rustix::fs::Stat) -> EntryMetadata {
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
    use std::ffi::OsString;

    #[cfg(unix)]
    fn fixture() -> tempfile::TempDir {
        fs::create_dir_all(".tmp").unwrap();
        tempfile::Builder::new()
            .prefix("backend-")
            .tempdir_in(".tmp")
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn portable_stream_keeps_metadata_anchored_and_bounds_batches() {
        use std::os::unix::fs::symlink;

        let directory = fixture();
        fs::create_dir(directory.path().join("root")).unwrap();
        fs::create_dir(directory.path().join("outside")).unwrap();
        fs::write(directory.path().join("root/file"), b"original").unwrap();
        fs::write(directory.path().join("root/.hidden"), b"hidden").unwrap();
        fs::write(directory.path().join("outside/file"), b"replacement").unwrap();
        symlink("../outside", directory.path().join("root/link")).unwrap();
        let anchor = RootAnchor::open(&directory.path().join("root")).unwrap();
        let reader = MetadataReader::Native(anchor.open_directory(Path::new("")).unwrap());
        fs::rename(
            directory.path().join("root"),
            directory.path().join("saved"),
        )
        .unwrap();
        symlink("outside", directory.path().join("root")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut seen = std::collections::HashMap::new();
        enumerate_portable(&reader, 1, &cancel, &mut |names, reader| {
            assert_eq!(names.len(), 1);
            for name in names {
                assert!(seen.insert(name.clone(), reader.metadata(&name)?).is_none());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[OsStr::new("file")].sizes.apparent, 8);
        assert_eq!(seen[OsStr::new("link")].kind, NodeKind::Symlink);

        let mut calls = 0;
        let error = enumerate_portable(&reader, 1, &cancel, &mut |_, _| {
            calls += 1;
            Err(io::Error::other("callback stopped"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "callback stopped");
        assert_eq!(calls, 1);
        cancel.store(true, Ordering::Relaxed);
        let error = enumerate_portable(&reader, 1, &cancel, &mut |_, _| {
            panic!("cancelled stream must not emit")
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn anchored_operations_reject_replaced_descendants_and_cancel_before_open() {
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
        assert!(anchor.open_parent(Path::new("branch/nested/file")).is_err());
        assert!(anchor.open_directory(Path::new("../outside")).is_err());
        assert!(anchor.open_parent(Path::new("")).is_err());
        let cancel = AtomicBool::new(true);
        for relative in [Path::new(""), Path::new("missing")] {
            let error = anchor
                .open_directory_cancellable(relative, &cancel)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
        let error = anchor
            .open_parent_cancellable(Path::new("missing/file"), &cancel)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
    #[test]
    fn unreadable_directory_anchor_keeps_identity_when_the_os_allows_it() {
        use std::os::unix::fs::PermissionsExt;

        let directory = fixture();
        let root = directory.path().join("root");
        fs::create_dir(&root).unwrap();
        let expected = root_metadata(&root).unwrap().identity;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o0)).unwrap();
        let result = RootAnchor::open(&root);
        // Restore permissions even when an unexpected open failure is asserted.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(target_os = "macos")]
        if let Err(error) = &result {
            // Darwin's inherited O_EVTONLY I/O policy can require read access.
            // Keep that failure closed instead of resolving through the path.
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            return;
        }
        let anchor = result.unwrap();
        assert_eq!(anchor.metadata().unwrap().identity, expected);
        fs::rename(&root, directory.path().join("saved")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("replacement"), b"outside the retained root").unwrap();
        assert_eq!(anchor.metadata().unwrap().identity, expected);
        enumerate_anchored(
            &anchor,
            Path::new(""),
            Backend::Auto,
            4096,
            1,
            &AtomicBool::new(false),
            |_, _| panic!("a replacement root must not supply directory entries"),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn leaf_roots_keep_their_kind_without_opening_or_traversing_targets() {
        use std::os::unix::fs::symlink;

        let directory = fixture();
        fs::write(directory.path().join("file"), b"payload").unwrap();
        symlink("file", directory.path().join("link")).unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(directory.path().join("socket")).unwrap();
        for (name, kind) in [
            ("file", NodeKind::File),
            ("link", NodeKind::Symlink),
            ("socket", NodeKind::Other),
        ] {
            let anchor = RootAnchor::open(&directory.path().join(name)).unwrap();
            assert_eq!(anchor.metadata().unwrap().kind, kind);
            assert!(anchor.open_directory(Path::new("")).is_err());
            assert!(anchor.open_parent(Path::new("child")).is_err());
        }
    }

    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn fifo_root_is_not_opened_for_reading() {
        let directory = fixture();
        let path = directory.path().join("fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let anchor = RootAnchor::open(&path).unwrap();
        assert_eq!(anchor.metadata().unwrap().kind, NodeKind::Other);
        assert!(anchor.open_directory(Path::new("")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_buffer_cache_preserves_read_window_and_bounded_ownership() {
        std::thread::spawn(|| {
            let large = NativeReadBuffer::new(64 * 1024).unwrap();
            let allocation = large.bytes.as_ptr();
            drop(large);
            let mut small = NativeReadBuffer::new(4096).unwrap();
            assert_eq!(small.bytes.as_ptr(), allocation);
            assert_eq!(small.spare_capacity_mut().len(), 4096);
            small.grow().unwrap();
            assert_eq!(small.spare_capacity_mut().len(), 8192);
            let nested = NativeReadBuffer::new(8192).unwrap();
            assert_ne!(small.bytes.as_ptr(), nested.bytes.as_ptr());
            drop(nested);
            drop(small);
            NATIVE_READ_BUFFER.with(|cache| {
                let capacity = cache.borrow().capacity();
                assert!((64 * 1024..=NATIVE_BUFFER_LIMIT).contains(&capacity));
            });
            let oversized = NativeReadBuffer::new(NATIVE_BUFFER_LIMIT + 4096).unwrap();
            drop(oversized);
            NATIVE_READ_BUFFER.with(|cache| assert_eq!(cache.borrow().capacity(), 0));
        })
        .join()
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_enumeration_callbacks_can_reenter_and_release_buffers_on_error() {
        let outer = fixture();
        let inner = fixture();
        for index in 0..4 {
            fs::write(outer.path().join(format!("outer-{index}")), b"outer").unwrap();
        }
        for index in 0..3 {
            fs::write(inner.path().join(format!("inner-{index}")), b"inner").unwrap();
        }
        std::thread::spawn(move || {
            let cancel = AtomicBool::new(false);
            let mut outer_count = 0;
            let mut inner_count = 0;
            enumerate(
                outer.path(),
                Backend::Native,
                4096,
                1,
                &cancel,
                |names, reader| {
                    if outer_count == 0 {
                        enumerate(
                            inner.path(),
                            Backend::Native,
                            64 * 1024,
                            2,
                            &cancel,
                            |names, reader| {
                                for name in names {
                                    assert_eq!(reader.metadata(&name)?.sizes.apparent, 5);
                                    inner_count += 1;
                                }
                                Ok(())
                            },
                        )?;
                    }
                    for name in names {
                        assert_eq!(reader.metadata(&name)?.sizes.apparent, 5);
                        outer_count += 1;
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!((outer_count, inner_count), (4, 3));
            let error = enumerate(outer.path(), Backend::Native, 4096, 1, &cancel, |_, _| {
                Err(io::Error::other("stop after the first batch"))
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "stop after the first batch");
            NATIVE_READ_BUFFER.with(|cache| {
                let capacity = cache.borrow().capacity();
                assert!((64 * 1024..=NATIVE_BUFFER_LIMIT).contains(&capacity));
            });
        })
        .join()
        .unwrap();
    }

    #[cfg(unix)]
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
        assert!(native.contains_key(raw_name.as_os_str()));
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn root_normalization_preserves_parent_resolution_and_rejects_alias_ancestors() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir_all(directory.path().join("real/child")).unwrap();
        symlink("real", directory.path().join("alias")).unwrap();
        let root = directory.path().join("real/./child//../.");
        let anchor = RootAnchor::open(&root).unwrap();
        assert_eq!(anchor.root_path(), directory.path().join("real/child/.."));
        assert_eq!(
            anchor.metadata().unwrap().identity,
            root_metadata(&directory.path().join("real"))
                .unwrap()
                .identity,
        );
        for suffix in ["alias/../real", "missing/../real"] {
            assert!(
                RootAnchor::open(&directory.path().join(suffix)).is_err(),
                "normalization must not erase an unresolved ancestor",
            );
        }
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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
                        assert_eq!(name.as_ref(), "payload");
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

    #[cfg(unix)]
    #[test]
    fn deletion_ancestors_retain_the_whole_logical_chain_after_root_replacement() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        let root = directory.path().join("root");
        fs::create_dir_all(root.join("branch/parent")).unwrap();
        let anchor = RootAnchor::open(&root).unwrap();
        let root_identity = anchor.metadata().unwrap().identity.unwrap();
        let branch = anchor.open_directory(Path::new("branch")).unwrap();
        let branch_identity = native_metadata(rustix::fs::fstat(&branch).unwrap())
            .identity
            .unwrap();
        let parent = anchor
            .open_parent(Path::new("branch/parent/target"))
            .unwrap();
        let parent_identity = native_metadata(rustix::fs::fstat(&parent).unwrap())
            .identity
            .unwrap();
        fs::rename(&root, directory.path().join("saved")).unwrap();
        fs::create_dir_all(directory.path().join("outside/branch/parent")).unwrap();
        symlink("outside", &root).unwrap();
        let ancestors = anchor
            .parent_ancestors_cancellable(
                Path::new("branch/parent/target"),
                parent_identity,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            ancestors,
            std::collections::HashSet::from([root_identity, branch_identity, parent_identity,])
        );
        assert_eq!(
            anchor
                .parent_ancestors_cancellable(
                    Path::new("target"),
                    root_identity,
                    &AtomicBool::new(false),
                )
                .unwrap(),
            std::collections::HashSet::from([root_identity])
        );
    }

    #[cfg(unix)]
    #[test]
    fn deletion_ancestor_walk_rejects_replaced_parents_symlinks_and_cancellation() {
        use std::os::unix::fs::symlink;
        let directory = fixture();
        fs::create_dir_all(directory.path().join("parent")).unwrap();
        let anchor = RootAnchor::open(directory.path()).unwrap();
        let parent = anchor.open_parent(Path::new("parent/target")).unwrap();
        let identity = native_metadata(rustix::fs::fstat(&parent).unwrap())
            .identity
            .unwrap();
        fs::rename(
            directory.path().join("parent"),
            directory.path().join("saved"),
        )
        .unwrap();
        fs::create_dir(directory.path().join("parent")).unwrap();
        let result = anchor.parent_ancestors_cancellable(
            Path::new("parent/target"),
            identity,
            &AtomicBool::new(false),
        );
        assert!(result.unwrap_err().to_string().contains("changed"));
        fs::remove_dir(directory.path().join("parent")).unwrap();
        symlink("saved", directory.path().join("parent")).unwrap();
        assert!(
            anchor
                .parent_ancestors_cancellable(
                    Path::new("parent/target"),
                    identity,
                    &AtomicBool::new(false),
                )
                .is_err()
        );
        assert_eq!(
            anchor
                .parent_ancestors_cancellable(
                    Path::new("saved/target"),
                    identity,
                    &AtomicBool::new(true),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        for path in ["", ".", "../target", "/target"] {
            assert!(
                anchor
                    .parent_ancestors_cancellable(
                        Path::new(path),
                        identity,
                        &AtomicBool::new(false),
                    )
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn deletion_ancestor_walk_rejects_repeated_directory_identity_and_leaf_roots() {
        let directory = fixture();
        fs::write(directory.path().join("leaf"), b"keep").unwrap();
        let anchor = RootAnchor::open(directory.path()).unwrap();
        let mut ancestors = std::collections::HashSet::new();
        let cancel = AtomicBool::new(false);
        record_parent_ancestor(&anchor.fd, &mut ancestors, &cancel).unwrap();
        let error = record_parent_ancestor(&anchor.fd, &mut ancestors, &cancel).unwrap_err();
        assert!(error.to_string().contains("ancestor directory"));
        assert_eq!(ancestors.len(), 1);
        let leaf = RootAnchor::open(&directory.path().join("leaf")).unwrap();
        assert_eq!(
            leaf.parent_ancestors_cancellable(
                Path::new("target"),
                anchor.metadata().unwrap().identity.unwrap(),
                &cancel,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::NotADirectory
        );
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
                        assert_eq!(name.as_ref(), raw_name.as_os_str());
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
