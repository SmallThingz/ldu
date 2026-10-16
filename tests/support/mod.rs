use std::{ffi::OsString, io, os::unix::ffi::OsStringExt, path::Path};

/// Keep raw-byte filename coverage where supported, while still exercising the
/// complete fixture on macOS filesystems that reject invalid UTF-8 names.
pub(crate) fn create_name(
    parent: &Path,
    raw: &[u8],
    utf8_name: &str,
    create: impl Fn(&Path) -> io::Result<()>,
) -> OsString {
    let name = OsString::from_vec(raw.to_vec());
    match create(&parent.join(&name)) {
        Ok(()) => name,
        Err(error)
            if cfg!(target_os = "macos")
                && error.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error())
                && std::str::from_utf8(raw).is_err() =>
        {
            eprintln!("fixture filesystem rejected {name:?}; using {utf8_name:?}");
            let name = OsString::from(utf8_name);
            let path = parent.join(&name);
            create(&path)
                .unwrap_or_else(|error| panic!("could not create fixture {path:?}: {error}"));
            name
        }
        Err(error) => panic!("could not create fixture {:?}: {error}", parent.join(name)),
    }
}
