//! Linux regular-file exports use atomic replacement. Other platforms and
//! explicit streaming endpoints retain their existing opened-file semantics.

use ldu::types::FileIdentity;
use std::{fs, io, path::Path};

fn is_scanned_root(metadata: &std::fs::Metadata, scanned: Option<FileIdentity>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        scanned == Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, scanned);
        false
    }
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}

fn check_root(
    metadata: &fs::Metadata,
    root: &Path,
    initial: Option<&fs::Metadata>,
    scanned: Option<FileIdentity>,
) -> io::Result<()> {
    if is_scanned_root(metadata, scanned)
        || initial.is_some_and(|root| same_file(metadata, root))
        || fs::metadata(root).is_ok_and(|root| same_file(metadata, &root))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export file must not replace the scan root",
        ));
    }
    Ok(())
}

fn preflight_root(
    output: &Path,
    root: &Path,
    initial: Option<&fs::Metadata>,
    scanned: Option<FileIdentity>,
) -> io::Result<()> {
    // Refuse a known source alias before a writable FIFO open can wait for a
    // reader. A changing path still needs the authoritative opened-file check.
    if let Ok(metadata) = fs::metadata(output) {
        check_root(&metadata, root, initial, scanned)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) use atomic::write_file;
#[cfg(not(target_os = "linux"))]
pub(crate) use streaming::write_file;

#[cfg(target_os = "linux")]
mod atomic {
    #[cfg(test)]
    use super::same_file;
    use super::{FileIdentity, check_root, preflight_root};
    use std::{
        ffi::OsString,
        fs::{self, File, Metadata},
        io::{self, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    fn directory_required(path: &Path) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes();
        bytes.ends_with(b"/") || bytes.ends_with(b"/.") || bytes == b"."
    }

    // None denotes a descriptor endpoint, whose open-file semantics must remain
    // streaming even when the descriptor currently refers to a regular file.
    fn destination(path: &Path) -> io::Result<Option<PathBuf>> {
        let mut path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        for links in 0..=40 {
            if directory_required(&path) {
                return path.canonicalize().map(Some);
            }
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return path.canonicalize().map(Some);
            };
            let parent = parent.canonicalize()?;
            if parent == Path::new("/dev/fd") {
                return Ok(None);
            }
            path = parent.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if rustix::fs::statfs(&parent).is_ok_and(|fs| fs.f_type == 0x9fa0) {
                        // procfs magic links cannot be reproduced by treating their
                        // readlink text (e.g. pipe:[123] or "(deleted)") as a path.
                        return Ok(None);
                    }
                    if links == 40 {
                        break;
                    }
                    let link = fs::read_link(&path)?;
                    path = parent.join(link);
                }
                Ok(_) => return Ok(Some(path)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Some(path)),
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many export destination symlinks",
        ))
    }

    fn same_stat(left: &Metadata, right: &rustix::fs::Stat) -> bool {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.st_dev && left.ino() == right.st_ino
    }

    struct Temporary {
        name: OsString,
        file: File,
    }
    struct Pending {
        directory: rustix::fd::OwnedFd,
        destination: OsString,
        temporary: Option<Temporary>,
        previous: Option<File>,
    }

    impl Pending {
        fn open(parent: &Path, name: OsString, previous: Option<File>) -> io::Result<Self> {
            let directory =
                ldu::backend::RootAnchor::open(parent)?.open_parent(Path::new(&name))?;
            Ok(Self {
                directory,
                destination: name,
                temporary: None,
                previous,
            })
        }

        fn matches(&self, name: &OsString, identity: &Metadata) -> bool {
            rustix::fs::statat(&self.directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|current| same_stat(identity, &current))
        }

        fn unchanged(&self) -> io::Result<bool> {
            if let Some(previous) = &self.previous {
                return Ok(self.matches(&self.destination, &previous.metadata()?));
            }
            match rustix::fs::statat(
                &self.directory,
                &self.destination,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Err(rustix::io::Errno::NOENT) => Ok(true),
                Err(error) => Err(error.into()),
                Ok(_) => Ok(false),
            }
        }

        fn make_temporary(&mut self) -> io::Result<()> {
            for _ in 0..128 {
                let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
                let name = OsString::from(format!(".ldu-export-{}-{sequence}", std::process::id()));
                if name == self.destination {
                    continue;
                }
                let result = {
                    use rustix::fs::{Mode, OFlags, openat};
                    // A replacement remains private until its original access policy
                    // is restored. New outputs inherit the directory policy and umask.
                    let mode = if self.previous.is_some() {
                        0o600
                    } else {
                        0o666
                    };
                    openat(
                        &self.directory,
                        &name,
                        OFlags::WRONLY
                            | OFlags::CLOEXEC
                            | OFlags::NOFOLLOW
                            | OFlags::CREATE
                            | OFlags::EXCL,
                        Mode::from_bits_truncate(mode),
                    )
                    .map(File::from)
                    .map_err(io::Error::from)
                };
                match result {
                    Ok(file) => {
                        self.temporary = Some(Temporary { name, file });
                        return Ok(());
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not reserve a temporary export file",
            ))
        }

        fn commit(&mut self) -> io::Result<()> {
            let temporary = self.temporary.as_ref().unwrap();
            if !self.matches(&temporary.name, &temporary.file.metadata()?) || !self.unchanged()? {
                return Err(io::Error::other("export destination changed while writing"));
            }
            // Rename replaces the final entry, never a symlink target. Concurrent
            // namespace changes between identity checks and rename are not a snapshot.
            rustix::fs::renameat(
                &self.directory,
                &temporary.name,
                &self.directory,
                &self.destination,
            )?;
            self.temporary = None;
            Ok(())
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(temporary) = &self.temporary
                && temporary
                    .file
                    .metadata()
                    .is_ok_and(|identity| self.matches(&temporary.name, &identity))
            {
                let _ = rustix::fs::unlinkat(
                    &self.directory,
                    &temporary.name,
                    rustix::fs::AtFlags::empty(),
                );
            }
        }
    }

    // Existing descriptor endpoints and nonregular files deliberately retain their
    // streaming semantics; their name is not an ordinary replaceable file.
    fn stream(mut file: File, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
        if file.metadata()?.is_file() {
            file.set_len(0)?;
        }
        write(&mut file)?;
        file.flush()
    }

    pub(crate) fn write_file(
        output: &Path,
        root: &Path,
        initial: Option<&Metadata>,
        scanned: Option<FileIdentity>,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        preflight_root(output, root, initial, scanned)?;
        // Preserve the OS meaning of the exact spelling, including trailing '/.'
        // and descriptor magic links. This open never creates or truncates a file.
        let previous = match fs::OpenOptions::new().write(true).open(output) {
            Ok(file) => {
                let metadata = file.metadata()?;
                check_root(&metadata, root, initial, scanned)?;
                if !metadata.is_file() {
                    return stream(file, write);
                }
                Some(file)
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && !directory_required(output) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let Some(output) = destination(output)? else {
            return stream(
                previous.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "export descriptor is unavailable")
                })?,
                write,
            );
        };
        if destination(root).is_ok_and(|root| root.as_ref() == Some(&output)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "export file must not replace the scan root",
            ));
        }
        let parent = output
            .parent()
            .ok_or_else(|| io::Error::other("export has no parent"))?;
        let name = output
            .file_name()
            .ok_or_else(|| io::Error::other("export has no filename"))?;
        let mut pending = Pending::open(parent, name.to_owned(), previous)?;
        if !pending.unchanged()? {
            return Err(io::Error::other(
                "export destination changed before writing",
            ));
        }
        pending.make_temporary()?;
        let file = &mut pending.temporary.as_mut().unwrap().file;
        write(file)?;
        file.flush()?;
        if let Some(previous) = &pending.previous {
            let metadata = previous.metadata()?;
            check_root(&metadata, root, initial, scanned)?;
            preserve_metadata(previous, file, &metadata)?;
        }
        file.sync_all()?;
        pending.commit()
    }

    fn preserve_metadata(previous: &File, file: &File, metadata: &Metadata) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temporary = file.metadata()?;
        if temporary.uid() != metadata.uid() || temporary.gid() != metadata.gid() {
            rustix::fs::fchown(
                file,
                Some(rustix::process::Uid::from_raw(metadata.uid())),
                Some(rustix::process::Gid::from_raw(metadata.gid())),
            )?;
        }
        preserve_attributes(previous, file)?;
        // Content writes invalidate privilege bits as well as file capabilities.
        file.set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() & !0o6000,
        ))
    }

    fn attribute_names(file: &File) -> io::Result<Vec<u8>> {
        let length = match rustix::fs::flistxattr(file, &mut [0_u8; 0][..]) {
            Ok(length) => length,
            Err(rustix::io::Errno::NOTSUP) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if length > 65_536 {
            return Err(io::Error::other("export attribute list exceeds 64 KiB"));
        }
        if length == 0 {
            return Ok(Vec::new());
        }
        let mut names = vec![0; length];
        let length = rustix::fs::flistxattr(file, names.as_mut_slice())?;
        names.truncate(length);
        Ok(names)
    }

    // Copy policy/attributes rather than silently retaining a different ACL inherited
    // by the temporary. Failure leaves the original export untouched. Attribute lists
    // and individual values are bounded; unusually large policies report an error.
    fn preserve_attributes(previous: &File, file: &File) -> io::Result<()> {
        use rustix::fs::{XattrFlags, fgetxattr, fremovexattr, fsetxattr};
        use std::{collections::HashSet, ffi::OsStr, os::unix::ffi::OsStrExt};
        let names = attribute_names(previous)?;
        let names: HashSet<&[u8]> = names
            .split(|&byte| byte == 0)
            .filter(|name| !name.is_empty())
            .collect();
        for name in attribute_names(file)?
            .split(|&byte| byte == 0)
            .filter(|name| !name.is_empty())
        {
            if !names.contains(name) {
                fremovexattr(file, OsStr::from_bytes(name))?;
            }
        }
        if names.is_empty() {
            return Ok(());
        }
        let mut value = vec![0; 65_536];
        let mut current = vec![0; 65_536];
        for name in names {
            // These are invalidated by an ordinary content write too; copying them
            // would attach capabilities or integrity signatures to different bytes.
            if matches!(
                name,
                b"security.capability" | b"security.ima" | b"security.evm"
            ) {
                continue;
            }
            let name = OsStr::from_bytes(name);
            let length = fgetxattr(previous, name, value.as_mut_slice())?;
            if fgetxattr(file, name, current.as_mut_slice())
                .is_ok_and(|size| size == length && current[..size] == value[..length])
            {
                continue;
            }
            fsetxattr(file, name, &value[..length], XattrFlags::empty())?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn fixture() -> tempfile::TempDir {
            fs::create_dir_all(".tmp").unwrap();
            tempfile::tempdir_in(".tmp").unwrap()
        }

        fn no_temporaries(directory: &Path) {
            assert!(fs::read_dir(directory).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ldu-export-")
            }));
        }

        #[test]
        fn failed_output_preserves_existing_bytes_and_removes_new_temporary_files() {
            let fixture = fixture();
            let existing = fixture.path().join("existing");
            let absent = fixture.path().join("absent");
            fs::write(&existing, b"previous complete export").unwrap();
            let before = fs::metadata(&existing).unwrap();
            for output in [&existing, &absent] {
                let result = write_file(output, fixture.path(), None, None, |file| {
                    file.write_all(b"incomplete replacement")?;
                    Err(io::Error::other("injected output failure"))
                });
                assert!(result.is_err());
                assert_eq!(fs::read(&existing).unwrap(), b"previous complete export");
                assert!(!absent.exists());
                no_temporaries(fixture.path());
            }
            assert!(same_file(&before, &fs::metadata(existing).unwrap()));
        }

        #[test]
        fn atomic_output_preserves_modes_symlinks_and_non_utf8_names() {
            use std::os::unix::{
                ffi::OsStringExt,
                fs::{PermissionsExt, symlink},
            };
            let fixture = fixture();
            let output = fixture
                .path()
                .join(OsString::from_vec(b"output-\xff".to_vec()));
            let alias = fixture.path().join("alias");
            let link = fixture.path().join("link");
            fs::write(&output, b"old").unwrap();
            fs::set_permissions(&output, fs::Permissions::from_mode(0o640)).unwrap();
            fs::hard_link(&output, &alias).unwrap();
            symlink(output.file_name().unwrap(), &link).unwrap();
            write_file(&link, fixture.path(), None, None, |file| {
                file.write_all(b"new")
            })
            .unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(&output).unwrap(), b"new");
            assert_eq!(fs::read(&alias).unwrap(), b"old");
            assert_eq!(
                fs::metadata(output).unwrap().permissions().mode() & 0o777,
                0o640
            );

            let dangling = fixture.path().join("dangling");
            let intermediate = fixture.path().join("intermediate");
            let new = fixture.path().join("new");
            symlink("intermediate", &dangling).unwrap();
            symlink("new", &intermediate).unwrap();
            write_file(&dangling, fixture.path(), None, None, |file| {
                file.write_all(b"created")
            })
            .unwrap();
            assert_eq!(fs::read(&new).unwrap(), b"created");
            assert!(
                fs::symlink_metadata(&dangling)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(
                fs::symlink_metadata(&intermediate)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let control = fixture.path().join("umask-control");
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&control)
                .unwrap();
            assert_eq!(
                fs::metadata(new).unwrap().permissions().mode() & 0o777,
                fs::metadata(control).unwrap().permissions().mode() & 0o777
            );
            no_temporaries(fixture.path());
        }

        #[test]
        fn atomic_output_refuses_root_aliases_without_calling_writer() {
            use std::os::unix::fs::symlink;
            let fixture = fixture();
            let root = fixture.path().join("input");
            let alias = fixture.path().join("alias");
            let link = fixture.path().join("link");
            fs::write(&root, b"input must survive").unwrap();
            fs::hard_link(&root, &alias).unwrap();
            symlink("input", &link).unwrap();
            let metadata = fs::metadata(&root).unwrap();
            for output in [&root, &alias, &link] {
                assert!(
                    write_file(output, &root, Some(&metadata), None, |_| panic!(
                        "writer reached"
                    ))
                    .is_err()
                );
                assert_eq!(fs::read(&root).unwrap(), b"input must survive");
            }
            no_temporaries(fixture.path());
        }

        #[test]
        fn changed_destination_is_preserved_and_temporary_is_removed() {
            let fixture = fixture();
            let output = fixture.path().join("output");
            for existing in [false, true] {
                if existing {
                    fs::write(&output, b"previous export").unwrap();
                }
                let result = write_file(&output, fixture.path(), None, None, |file| {
                    file.write_all(b"new export")?;
                    if existing {
                        fs::remove_file(&output)?;
                    }
                    fs::write(&output, b"concurrent replacement")
                });
                assert!(result.is_err());
                assert_eq!(fs::read(&output).unwrap(), b"concurrent replacement");
                no_temporaries(fixture.path());
                fs::remove_file(&output).unwrap();
            }
        }

        #[test]
        fn parent_rename_does_not_redirect_atomic_commit() {
            let fixture = fixture();
            let parent = fixture.path().join("parent");
            let retained = fixture.path().join("retained");
            fs::create_dir(&parent).unwrap();
            let output = parent.join("output");
            fs::write(&output, b"old").unwrap();
            write_file(&output, fixture.path(), None, None, |file| {
                file.write_all(b"new")?;
                fs::rename(&parent, &retained)?;
                fs::create_dir(&parent)?;
                fs::write(parent.join("output"), b"replacement parent")
            })
            .unwrap();
            assert_eq!(
                fs::read(parent.join("output")).unwrap(),
                b"replacement parent"
            );
            assert_eq!(fs::read(retained.join("output")).unwrap(), b"new");
            no_temporaries(&parent);
            no_temporaries(&retained);
        }

        #[test]
        fn writable_devices_keep_streaming_without_truncation_or_sync() {
            write_file(Path::new("/dev/null"), Path::new("/"), None, None, |file| {
                file.write_all(b"device output")
            })
            .unwrap();
        }
    }

    #[cfg(test)]
    mod linux_regressions {
        use super::*;
        use std::{
            io::{Read, Seek},
            os::fd::AsRawFd,
        };

        fn fixture() -> tempfile::TempDir {
            fs::create_dir_all(".tmp").unwrap();
            tempfile::tempdir_in(".tmp").unwrap()
        }

        #[test]
        fn descriptor_output_keeps_pipe_and_regular_file_semantics() {
            let fixture = fixture();
            let (mut reader, writer) = std::io::pipe().unwrap();
            let path = PathBuf::from(format!("/proc/self/fd/{}", writer.as_raw_fd()));
            let alias = fixture.path().join("pipe-alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            write_file(&alias, fixture.path(), None, None, |file| {
                file.write_all(b"pipe bytes")
            })
            .unwrap();
            drop(writer);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"pipe bytes");

            let path = fixture.path().join("held");
            let mut held = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let before = held.metadata().unwrap();
            let endpoint = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));
            for deleted in [false, true] {
                if deleted {
                    fs::remove_file(&path).unwrap();
                }
                write_file(&endpoint, fixture.path(), None, None, |file| {
                    file.write_all(b"descriptor bytes")
                })
                .unwrap();
                held.rewind().unwrap();
                bytes.clear();
                held.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"descriptor bytes");
                assert!(same_file(&before, &held.metadata().unwrap()));
            }
        }

        #[test]
        fn directory_suffixes_never_turn_into_regular_output_files() {
            let fixture = fixture();
            let existing = fixture.path().join("existing");
            fs::write(&existing, b"keep").unwrap();
            for name in ["existing/", "existing/.", "absent/", "absent/."] {
                assert!(
                    write_file(
                        &fixture.path().join(name),
                        fixture.path(),
                        None,
                        None,
                        |_| panic!("directory spelling reached writer")
                    )
                    .is_err()
                );
                assert_eq!(fs::read(&existing).unwrap(), b"keep");
                assert!(!fixture.path().join("absent").exists());
            }
            let link = fixture.path().join("link");
            std::os::unix::fs::symlink("absent/", &link).unwrap();
            assert!(
                write_file(&link, fixture.path(), None, None, |_| panic!(
                    "dangling directory link reached writer"
                ))
                .is_err()
            );
            assert!(!fixture.path().join("absent").exists());
        }

        #[test]
        fn replaced_temporary_is_neither_published_nor_removed() {
            let fixture = fixture();
            let output = fixture.path().join("output");
            fs::write(&output, b"old export").unwrap();
            let mut replacement = None;
            let result = write_file(&output, fixture.path(), None, None, |file| {
                file.write_all(b"new export")?;
                let temporary = fs::read_dir(fixture.path())?
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".ldu-export-")
                    })
                    .unwrap();
                fs::remove_file(&temporary)?;
                fs::write(&temporary, b"unrelated replacement")?;
                replacement = Some(temporary);
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(fs::read(&output).unwrap(), b"old export");
            assert_eq!(
                fs::read(replacement.unwrap()).unwrap(),
                b"unrelated replacement"
            );
        }
    }

    #[cfg(test)]
    mod acl_regressions {
        use super::*;
        use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
        use std::os::unix::fs::PermissionsExt;

        fn fixture() -> tempfile::TempDir {
            fs::create_dir_all(".tmp").unwrap();
            tempfile::tempdir_in(".tmp").unwrap()
        }

        fn policy() -> Vec<u8> {
            let uid = u32::from(rustix::process::geteuid().as_raw() == 0);
            let mut bytes = 2u32.to_le_bytes().to_vec();
            // Owner rw, named user r, owning group none, mask r, others none.
            // Its group mode bits are the ACL mask, so copying mode alone would
            // grant the owning group read access it did not previously possess.
            for (tag, permissions, id) in [
                (1u16, 6u16, u32::MAX),
                (2, 4, uid),
                (4, 0, u32::MAX),
                (16, 4, u32::MAX),
                (32, 0, u32::MAX),
            ] {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&permissions.to_le_bytes());
                bytes.extend_from_slice(&id.to_le_bytes());
            }
            bytes
        }

        #[test]
        fn replacement_preserves_acl_and_user_attributes() {
            let fixture = fixture();
            let output = fixture.path().join("output");
            fs::write(&output, b"old").unwrap();
            let old = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&output)
                .unwrap();
            let acl = policy();
            match fsetxattr(&old, "system.posix_acl_access", &acl, XattrFlags::empty()) {
                Ok(()) => (),
                Err(rustix::io::Errno::NOTSUP) => return,
                Err(error) => panic!("cannot set fixture ACL: {error}"),
            }
            fsetxattr(
                &old,
                "user.ldu-test",
                b"retained attribute",
                XattrFlags::empty(),
            )
            .unwrap();
            write_file(&output, fixture.path(), None, None, |file| {
                file.write_all(b"new")
            })
            .unwrap();
            let new = File::open(&output).unwrap();
            let mut value = [0; 256];
            let length = fgetxattr(&new, "system.posix_acl_access", &mut value[..]).unwrap();
            assert_eq!(&value[..length], acl);
            let length = fgetxattr(&new, "user.ldu-test", &mut value[..]).unwrap();
            assert_eq!(&value[..length], b"retained attribute");
            assert_eq!(fs::read(output).unwrap(), b"new");
        }

        #[test]
        fn replacement_does_not_gain_parent_default_acl() {
            let fixture = fixture();
            let output = fixture.path().join("output");
            fs::write(&output, b"old").unwrap();
            fs::set_permissions(&output, fs::Permissions::from_mode(0o640)).unwrap();
            let parent = File::open(fixture.path()).unwrap();
            match fsetxattr(
                &parent,
                "system.posix_acl_default",
                &policy(),
                XattrFlags::empty(),
            ) {
                Ok(()) => (),
                Err(rustix::io::Errno::NOTSUP) => return,
                Err(error) => panic!("cannot set fixture default ACL: {error}"),
            }
            write_file(&output, fixture.path(), None, None, |file| {
                file.write_all(b"new")
            })
            .unwrap();
            let new = File::open(&output).unwrap();
            let mut value = [0; 256];
            assert_eq!(
                fgetxattr(&new, "system.posix_acl_access", &mut value[..]),
                Err(rustix::io::Errno::NODATA)
            );
            assert_eq!(new.metadata().unwrap().permissions().mode() & 0o777, 0o640);
        }
        #[test]
        fn replacement_clears_content_invalidated_privilege_bits() {
            let fixture = fixture();
            let output = fixture.path().join("output");
            fs::write(&output, b"old").unwrap();
            fs::set_permissions(&output, fs::Permissions::from_mode(0o6750)).unwrap();
            write_file(&output, fixture.path(), None, None, |file| {
                file.write_all(b"new")
            })
            .unwrap();
            assert_eq!(
                fs::metadata(output).unwrap().permissions().mode() & 0o7777,
                0o750
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod streaming {
    use super::{FileIdentity, check_root, preflight_root};
    use std::{
        fs::{self, File, Metadata, OpenOptions},
        io::{self, Write},
        path::{Path, PathBuf},
    };
    // Resolve existing aliases and also compare destinations whose leaf is absent.
    fn destination(path: &Path) -> io::Result<PathBuf> {
        path.canonicalize().or_else(|_| {
            let absolute = if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            };
            match (absolute.parent(), absolute.file_name()) {
                (Some(parent), Some(name)) => Ok(parent.canonicalize()?.join(name)),
                _ => Ok(absolute),
            }
        })
    }

    fn create_export(
        output: &Path,
        root: &Path,
        initial: Option<&Metadata>,
        scanned: Option<FileIdentity>,
    ) -> io::Result<File> {
        preflight_root(output, root, initial, scanned)?;
        let refused = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "export file must not replace the scan root",
            )
        };
        if let (Ok(output), Ok(root)) = (destination(output), destination(root))
            && output == root
        {
            return Err(refused());
        }
        // Check the opened file before truncating it: pathname-only checks miss
        // hardlinks, and following an output symlink must not truncate the root.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(output)?;
        let metadata = file.metadata()?;
        check_root(&metadata, root, initial, scanned)?;
        if metadata.is_file() {
            file.set_len(0)?;
        }
        Ok(file)
    }

    pub(crate) fn write_file(
        output: &Path,
        root: &Path,
        initial: Option<&Metadata>,
        scanned: Option<FileIdentity>,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut file = create_export(output, root, initial, scanned)?;
        write(&mut file)?;
        file.flush()
    }
}

#[cfg(all(test, unix))]
mod root_identity_tests {
    use super::*;
    use std::{fs, path::PathBuf};

    #[test]
    fn export_guard_retains_the_scanned_identity_between_path_snapshots() {
        fs::create_dir_all(".tmp").unwrap();
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let original = fixture.path().join("original");
        let scanned = fixture.path().join("scanned");
        let displayed = fixture.path().join("displayed");
        let alias = fixture.path().join("alias");
        fs::write(&original, b"initial path object").unwrap();
        fs::write(&scanned, b"actual scanned contents").unwrap();
        fs::write(&displayed, b"current path object").unwrap();
        fs::hard_link(&scanned, &alias).unwrap();
        let initial = fs::metadata(&original).unwrap();
        let anchor = ldu::backend::RootAnchor::open(&scanned).unwrap();
        let identity = anchor.metadata().unwrap().identity;
        assert!(identity.is_some());
        assert!(!is_scanned_root(&initial, identity));
        assert!(!is_scanned_root(
            &fs::metadata(&displayed).unwrap(),
            identity
        ));
        let outputs: Vec<PathBuf> = vec![scanned.clone(), alias];
        #[cfg(target_os = "linux")]
        let held = fs::File::open(&scanned).unwrap();
        #[cfg(target_os = "linux")]
        let outputs = {
            use std::os::fd::AsRawFd;
            let mut outputs = outputs;
            outputs.push(PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd())));
            outputs
        };
        for output in outputs {
            let result = write_file(&output, &displayed, Some(&initial), identity, |_| {
                panic!("scanned root alias reached the writer")
            });
            assert!(result.is_err());
            assert_eq!(fs::read(&scanned).unwrap(), b"actual scanned contents");
        }
    }

    #[test]
    fn fifo_root_aliases_are_rejected_by_preflight_without_opening() {
        fs::create_dir_all(".tmp").unwrap();
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let source = fixture.path().join("source-fifo");
        let unrelated = fixture.path().join("output-fifo");
        let original = fixture.path().join("original");
        let displayed = fixture.path().join("displayed");
        fs::write(&original, b"initial path object").unwrap();
        fs::write(&displayed, b"current path object").unwrap();
        for fifo in [&source, &unrelated] {
            #[cfg(not(target_vendor = "apple"))]
            rustix::fs::mknodat(
                rustix::fs::CWD,
                fifo,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                0,
            )
            .unwrap();
            #[cfg(target_vendor = "apple")]
            assert!(
                std::process::Command::new("mkfifo")
                    .args(["-m", "600"])
                    .arg(fifo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let alias = fixture.path().join("hardlink");
        let link = fixture.path().join("symlink");
        fs::hard_link(&source, &alias).unwrap();
        std::os::unix::fs::symlink("source-fifo", &link).unwrap();
        let initial = fs::metadata(original).unwrap();
        let anchor = ldu::backend::RootAnchor::open(&source).unwrap();
        let scanned = anchor.metadata().unwrap().identity;
        assert!(scanned.is_some());
        for output in [&source, &alias, &link] {
            assert!(preflight_root(output, &displayed, Some(&initial), scanned).is_err());
        }
        // Explicit unrelated FIFO output remains a valid streaming destination.
        assert!(preflight_root(&unrelated, &displayed, Some(&initial), scanned).is_ok());
    }
}
