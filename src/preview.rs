//! Bounded, latest-selection file previews. All filesystem work stays off the UI.
use crate::{
    backend::RootAnchor,
    types::{NodeId, NodeKind},
};

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
#[path = "preview_helpers.rs"]
mod helpers;
use arc_swap::ArcSwapOption;
use crossbeam_channel::{Sender, bounded};
use std::{
    borrow::Cow,
    io::{self, Read},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const READ_LIMIT: usize = 64 * 1024;
const TEXT_LIMIT: usize = 32 * 1024;
const LINE_LIMIT: usize = 256;

struct Request {
    id: NodeId,
    revision: u64,
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    kind: NodeKind,
    cancelled: AtomicBool,
}

pub struct FileContents {
    revision: u64,
    pub kind: &'static str,
    pub text: Option<String>,
    pub truncated: bool,
}

pub struct FilePreview {
    request: Arc<ArcSwapOption<Request>>,
    result: Arc<ArcSwapOption<FileContents>>,
    wake: Sender<()>,
    revision: u64,
    stopped: bool,
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    helper_control: Arc<helpers::Control>,
}

impl FilePreview {
    pub fn new() -> io::Result<Self> {
        let request: Arc<ArcSwapOption<Request>> = Arc::new(ArcSwapOption::empty());
        let result: Arc<ArcSwapOption<FileContents>> = Arc::new(ArcSwapOption::empty());
        let (wake, receiver) = bounded(1);
        let requests = Arc::clone(&request);
        let results = Arc::clone(&result);
        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        let helper_control = Arc::new(helpers::Control::default());
        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        let control = Arc::clone(&helper_control);
        std::thread::Builder::new()
            .name("ldu-preview".into())
            .spawn(move || {
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                let mut helpers = None;
                while receiver.recv().is_ok() {
                    let Some(request) = requests.load_full() else {
                        continue;
                    };
                    if results
                        .load()
                        .as_ref()
                        .is_some_and(|contents| contents.revision == request.revision)
                    {
                        continue;
                    }
                    let mut contents = read_contents(
                        &request,
                        #[cfg(any(
                            target_os = "linux",
                            target_os = "macos",
                            target_os = "freebsd",
                            target_os = "dragonfly"
                        ))]
                        &mut helpers,
                        #[cfg(any(
                            target_os = "linux",
                            target_os = "macos",
                            target_os = "freebsd",
                            target_os = "dragonfly"
                        ))]
                        &control,
                    )
                    .unwrap_or(FileContents {
                        revision: 0,
                        kind: "Unavailable",
                        text: None,
                        truncated: false,
                    });
                    if !request.cancelled.load(Ordering::Relaxed)
                        && requests
                            .load()
                            .as_ref()
                            .is_some_and(|latest| latest.revision == request.revision)
                    {
                        contents.revision = request.revision;
                        results.store(Some(Arc::new(contents)));
                    }
                }
            })?;
        Ok(Self {
            request,
            result,
            wake,
            revision: 0,
            stopped: false,
            #[cfg(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "freebsd",
                target_os = "dragonfly"
            ))]
            helper_control,
        })
    }

    pub fn selected(&self) -> Option<NodeId> {
        self.request.load().as_ref().map(|request| request.id)
    }

    pub fn matches(&self, id: NodeId, kind: NodeKind) -> bool {
        self.request
            .load()
            .as_ref()
            .is_some_and(|request| request.id == id && request.kind == kind)
    }

    pub fn request(
        &mut self,
        id: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
        kind: NodeKind,
    ) {
        if self.stopped {
            return;
        }
        if self.matches(id, kind) {
            return;
        }
        self.clear();
        self.revision = self.revision.wrapping_add(1);
        self.request.store(Some(Arc::new(Request {
            id,
            revision: self.revision,
            anchor,
            relative,
            kind,
            cancelled: AtomicBool::new(false),
        })));
        let _ = self.wake.try_send(());
    }

    pub fn clear(&mut self) {
        if let Some(previous) = self.request.swap(None) {
            previous.cancelled.store(true, Ordering::Relaxed);
            #[cfg(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "freebsd",
                target_os = "dragonfly"
            ))]
            self.helper_control.cancel();
        }
        self.result.store(None);
    }

    /// Stop future helper launches and synchronously signal any owned helper.
    /// Never wait for a filesystem operation, process spawn, worker, or process reap.
    pub fn shutdown(&mut self) {
        self.stopped = true;
        self.clear();
        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        self.helper_control.shutdown();
    }

    pub fn snapshot(&self) -> Option<Arc<FileContents>> {
        let request = self.request.load();
        let revision = request.as_ref()?.revision;
        self.result
            .load_full()
            .filter(|contents| contents.revision == revision)
    }

    /// Check for a new publication without cloning the retained contents.
    pub fn snapshot_matches(&self, expected: Option<&Arc<FileContents>>) -> bool {
        let request = self.request.load();
        let result = self.result.load();
        let current = request.as_ref().and_then(|request| {
            result
                .as_ref()
                .filter(|contents| contents.revision == request.revision)
        });
        current.map(Arc::as_ptr) == expected.map(Arc::as_ptr)
    }
}

impl Drop for FilePreview {
    fn drop(&mut self) {
        self.shutdown();
        // Dropping the sender wakes an idle worker. Never join a blocked read.
    }
}

fn cancelled(request: &Request) -> io::Result<()> {
    if request.cancelled.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "preview cancelled",
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn open_parent(request: &Request) -> io::Result<rustix::fd::OwnedFd> {
    use std::path::Component;
    if request.relative.file_name().is_none()
        || request
            .relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::other("invalid preview path"));
    }
    cancelled(request)?;
    let parent = request
        .anchor
        .open_parent_cancellable(&request.relative, &request.cancelled)?;
    Ok(parent)
}

#[cfg(unix)]
fn open_file(request: &Request) -> io::Result<(std::fs::File, u64)> {
    use rustix::fs::{Mode, OFlags, openat};
    let parent = open_parent(request)?;
    let name = request
        .relative
        .file_name()
        .ok_or_else(|| io::Error::other("preview requires a file"))?;
    let fd = loop {
        cancelled(request)?;
        match openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Err(rustix::io::Errno::INTR) => continue,
            result => break result?,
        }
    };
    let file = std::fs::File::from(fd);
    cancelled(request)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("preview requires a regular file"));
    }
    Ok((file, metadata.len()))
}

#[cfg(not(unix))]
fn open_file(_request: &Request) -> io::Result<(std::fs::File, u64)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file preview unavailable",
    ))
}

#[cfg(unix)]
fn read_link(request: &Request) -> io::Result<FileContents> {
    let parent = open_parent(request)?;
    let name = request
        .relative
        .file_name()
        .ok_or_else(|| io::Error::other("preview requires a link"))?;
    // A fixed buffer also bounds unusual synthetic/filesystem link targets.
    let mut bytes = vec![0; READ_LIMIT];
    let count = loop {
        cancelled(request)?;
        match rustix::fs::readlinkat_raw(&parent, name, &mut bytes[..]) {
            Err(rustix::io::Errno::INTR) => continue,
            result => break result?,
        }
    };
    cancelled(request)?;
    let (text, escaped_truncated) = escape_path(&bytes[..count]);
    Ok(FileContents {
        revision: 0,
        kind: "Symlink",
        text: Some(text),
        truncated: count == READ_LIMIT || escaped_truncated,
    })
}

#[cfg(not(unix))]
fn read_link(_request: &Request) -> io::Result<FileContents> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "link preview unavailable",
    ))
}

// Preserve the stored destination literally: no resolution and no lossy byte replacement.
fn escape_path(mut bytes: &[u8]) -> (String, bool) {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len().min(TEXT_LIMIT));
    let mut piece = String::with_capacity(10);
    while !bytes.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(bytes) {
            Ok(text) => (text, 0),
            Err(error) => (
                std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap(),
                error
                    .error_len()
                    .unwrap_or(bytes.len() - error.valid_up_to()),
            ),
        };
        for ch in valid.chars() {
            piece.clear();
            match ch {
                '\\' => piece.push_str("\\\\"),
                '\n' => piece.push_str("\\n"),
                '\r' => piece.push_str("\\r"),
                '\t' => piece.push_str("\\t"),
                ch if ch.is_control()
                    || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}') =>
                {
                    let _ = write!(piece, "\\u{{{:x}}}", u32::from(ch));
                }
                ch => piece.push(ch),
            }
            if output.len() + piece.len() > TEXT_LIMIT {
                return (output, true);
            }
            output.push_str(&piece);
        }
        bytes = &bytes[valid.len()..];
        for byte in &bytes[..invalid] {
            if output.len() + 4 > TEXT_LIMIT {
                return (output, true);
            }
            let _ = write!(output, "\\x{byte:02x}");
        }
        bytes = &bytes[invalid..];
    }
    (output, false)
}

fn read_contents(
    request: &Request,
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    helpers: &mut Option<helpers::Catalog>,
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    control: &helpers::Control,
) -> io::Result<FileContents> {
    cancelled(request)?;
    if request.kind == NodeKind::Symlink {
        return read_link(request);
    }
    if request.kind != NodeKind::File {
        return Err(io::Error::other("unsupported preview kind"));
    }
    let (mut file, length) = open_file(request)?;
    let (bytes, truncated) = read_prefix(&mut file, length, || cancelled(request))?;
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    if let Some(contents) = helpers::preview(
        &mut file,
        &bytes,
        &request.relative,
        &request.cancelled,
        helpers,
        control,
    )? {
        return Ok(contents);
    }
    Ok(decode(&bytes, truncated))
}

fn read_prefix(
    mut reader: impl Read,
    length_hint: u64,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(length_hint.min(READ_LIMIT as u64) as usize);
    let mut chunk = [0; 4096];
    let mut binary = false;
    while bytes.len() < READ_LIMIT {
        check()?;
        let limit = chunk.len().min(READ_LIMIT - bytes.len());
        let count = match reader.read(&mut chunk[..limit]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        bytes.extend_from_slice(&chunk[..count]);
        binary = binary || chunk[..count].contains(&0);
        // Short reads can split image headers containing NUL before their magic.
        if file_kind(&bytes) != "Binary" || (bytes.len() >= 32 && binary && !utf16(&bytes)) {
            break;
        }
    }
    check()?;
    let truncated = bytes.len() == READ_LIMIT;
    Ok((bytes, truncated))
}

fn utf16(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xff\xfe") || bytes.starts_with(b"\xfe\xff")
}

fn archive_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"Rar!\x1a\x07")
        || bytes.starts_with(b"\x1f\x8b")
        || bytes.starts_with(b"BZh")
        || bytes.starts_with(b"\xfd7zXZ\0")
        || bytes.starts_with(b"\x28\xb5\x2f\xfd")
        || bytes.get(257..262) == Some(b"ustar")
        || bytes.starts_with(b"!<arch>\n")
        || bytes.starts_with(b"070701")
        || bytes.starts_with(b"070702")
        || bytes.starts_with(b"070707")
}

fn file_kind(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        "PDF"
    } else if archive_magic(bytes) {
        "Archive"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"BM") && bytes.len() >= 26 && bytes.get(6..10) == Some(&[0; 4]))
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"\0\0\x01\0")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
    {
        "Image"
    } else {
        "Binary"
    }
}

fn decode(bytes: &[u8], truncated: bool) -> FileContents {
    let kind = file_kind(bytes);
    let fallback = || FileContents {
        revision: 0,
        kind,
        text: None,
        truncated: false,
    };
    if kind != "Binary" {
        return fallback();
    }
    let text = if utf16(bytes) {
        let little = bytes[0] == 0xff;
        let bytes = &bytes[2..];
        if !bytes.len().is_multiple_of(2) && !truncated {
            return fallback();
        }
        let mut units: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        if truncated
            && units
                .last()
                .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            units.pop();
        }
        let Ok(text) = String::from_utf16(&units) else {
            return fallback();
        };
        Cow::Owned(text)
    } else {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        match std::str::from_utf8(bytes) {
            Ok(text) => Cow::Borrowed(text),
            Err(error) if truncated && error.error_len().is_none() => {
                Cow::Borrowed(std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap())
            }
            Err(_) => return fallback(),
        }
    };
    if text.contains('\0') {
        return fallback();
    }
    text_contents(&text, truncated)
}

fn text_contents(text: &str, mut truncated: bool) -> FileContents {
    let mut output = String::with_capacity(text.len().min(TEXT_LIMIT));
    let mut lines = 1;
    let mut chars = text.chars().peekable();
    let mut escaped = String::with_capacity(10);
    while let Some(ch) = chars.next() {
        escaped.clear();
        let piece = match ch {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\r' | '\n' => {
                if lines >= LINE_LIMIT {
                    truncated = true;
                    break;
                }
                lines += 1;
                "\n"
            }
            '\t' => "    ",
            ch if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}') =>
            {
                use std::fmt::Write;
                let _ = write!(escaped, "\\u{{{:x}}}", u32::from(ch));
                escaped.as_str()
            }
            _ => {
                escaped.push(ch);
                escaped.as_str()
            }
        };
        if output.len() + piece.len() > TEXT_LIMIT {
            truncated = true;
            break;
        }
        output.push_str(piece);
    }
    FileContents {
        revision: 0,
        kind: "Text",
        text: Some(output),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_decoding_sanitizes_controls_and_bounds_output() {
        for bytes in [
            b"hello\tworld\r\n\x1b[31m".to_vec(),
            [
                b"\xff\xfe".as_slice(),
                &"hello\tworld\r\n\u{1b}[31m"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            ]
            .concat(),
            [
                b"\xfe\xff".as_slice(),
                &"hello\tworld\r\n\u{1b}[31m"
                    .encode_utf16()
                    .flat_map(u16::to_be_bytes)
                    .collect::<Vec<_>>(),
            ]
            .concat(),
        ] {
            let result = decode(&bytes, false);
            assert_eq!(result.kind, "Text");
            assert_eq!(result.text.as_deref(), Some("hello    world\n\\u{1b}[31m"));
        }
        for bytes in [
            b"%PDF-1.7".as_slice(),
            b"\x89PNG\r\n\x1a\n",
            b"\0binary",
            b"\xffinvalid",
        ] {
            assert!(decode(bytes, false).text.is_none());
        }
        for bytes in [vec![b'a'; READ_LIMIT], b"x\n".repeat(LINE_LIMIT + 10)] {
            let result = decode(&bytes, false);
            let text = result.text.unwrap();
            assert!(result.truncated && text.len() <= TEXT_LIMIT);
            assert!(text.lines().count() <= LINE_LIMIT);
        }
        let result = decode(b"hello \xe2\x82", true);
        assert_eq!(result.text.as_deref(), Some("hello "));
        assert!(result.truncated);
        assert!(decode(b"hello \xe2\x82", false).text.is_none());
        assert_eq!(
            decode("safe\u{202e}tail".as_bytes(), false).text.as_deref(),
            Some("safe\\u{202e}tail")
        );
        struct Counted<'a> {
            bytes: &'a [u8],
            chunk: usize,
            reads: usize,
        }
        impl Read for Counted<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                let count = self.bytes.len().min(output.len()).min(self.chunk);
                output[..count].copy_from_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                Ok(count)
            }
        }
        let bounded = |input: &[u8], hint, chunk| {
            let mut reader = Counted {
                bytes: input,
                chunk,
                reads: 0,
            };
            let (bytes, truncated) = read_prefix(&mut reader, hint, || Ok(())).unwrap();
            (decode(&bytes, truncated), bytes.len(), reader.reads)
        };
        for hint in [0, 1, 4096, u64::MAX] {
            let (contents, len, reads) = bounded(b"hello", hint, 4096);
            assert_eq!((len, reads, contents.truncated), (5, 2, false));
            assert_eq!(contents.text.as_deref(), Some("hello"));
            let (contents, _, _) = bounded(b"hello \xe2\x82", hint, 4096);
            assert!(contents.text.is_none());
        }
        let large = [b"\xff\xfe".as_slice(), &b"a\0".repeat(READ_LIMIT / 2)].concat();
        for end in [READ_LIMIT, large.len()] {
            for hint in [0, 1, u64::MAX] {
                let (contents, len, reads) = bounded(&large[..end], hint, 4096);
                assert_eq!((len, reads), (READ_LIMIT, READ_LIMIT / 4096));
                assert_eq!(contents.text.unwrap().len(), READ_LIMIT / 2 - 1);
                assert!(contents.truncated);
            }
        }
        for chunk in [1, 4096] {
            for (bytes, text) in [
                (b"\xff\xfea\0b\0".as_slice(), "ab"),
                (b"\xfe\xff\0a\0b", "ab"),
                ("\u{20ac}".as_bytes(), "\u{20ac}"),
            ] {
                let (contents, len, reads) = bounded(bytes, u64::MAX, chunk);
                assert_eq!(contents.text.as_deref(), Some(text));
                assert!(!contents.truncated);
                assert_eq!((len, reads), (bytes.len(), bytes.len().div_ceil(chunk) + 1));
            }
            let mut bmp = [0; 26];
            bmp[..2].copy_from_slice(b"BM");
            for (prefix, kind) in [
                (b"binary\0".as_slice(), "Binary"),
                (b"%PDF-", "PDF"),
                (b"\x89PNG\r\n\x1a\n", "Image"),
                (b"\0\0\x01\0", "Image"),
                (b"RIFF\0\0\0\0WEBP", "Image"),
                (bmp.as_slice(), "Image"),
            ] {
                let mut input = vec![b'x'; READ_LIMIT];
                input[..prefix.len()].copy_from_slice(prefix);
                let (contents, len, reads) = bounded(&input, 1, chunk);
                assert_eq!(contents.kind, kind);
                assert!(contents.text.is_none());
                if chunk == 1 {
                    assert!(reads <= 32 && len <= 32);
                } else {
                    assert_eq!((len, reads), (4096, 1));
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn anchored_reads_reject_links_and_stale_results() {
        std::fs::create_dir_all(".tmp").unwrap();
        let directory = tempfile::tempdir_in(".tmp").unwrap();
        let root = directory.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("first"), "first").unwrap();
        std::fs::write(root.join("last"), "last").unwrap();
        std::os::unix::fs::symlink("first", root.join("link")).unwrap();
        std::os::unix::fs::symlink(".", root.join("parent")).unwrap();
        let anchor = Arc::new(RootAnchor::open(&root).unwrap());
        for path in ["link", "parent/first", ".", "../first"] {
            let request = Request {
                id: 1,
                revision: 1,
                anchor: Arc::clone(&anchor),
                relative: PathBuf::from(path),
                kind: NodeKind::File,
                cancelled: AtomicBool::new(false),
            };
            assert!(
                read_contents(
                    &request,
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &mut Some(helpers::Catalog::empty()),
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &helpers::Control::default()
                )
                .is_err(),
                "{path}"
            );
        }
        let mut preview = FilePreview::new().unwrap();
        preview.request(
            1,
            Arc::clone(&anchor),
            PathBuf::from("first"),
            NodeKind::File,
        );
        preview.clear();
        assert!(preview.selected().is_none() && preview.snapshot().is_none());
        assert!(preview.snapshot_matches(None));
        preview.request(
            2,
            Arc::clone(&anchor),
            PathBuf::from("last"),
            NodeKind::File,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while preview.snapshot().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let contents = preview.snapshot().unwrap();
        assert_eq!(contents.text.as_deref(), Some("last"));
        assert!(preview.snapshot_matches(Some(&contents)));
        assert!(!preview.snapshot_matches(None));
        {
            std::fs::rename(&root, directory.path().join("moved")).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("last"), "replacement").unwrap();
            #[cfg(not(target_vendor = "apple"))]
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                directory.path().join("moved/fifo"),
                rustix::fs::Mode::RUSR,
            )
            .unwrap();
            #[cfg(target_vendor = "apple")]
            {
                // Keep FIFO rejection covered without unsafe Darwin syscall glue.
                let output = std::process::Command::new("mkfifo")
                    .args(["-m", "400"])
                    .arg(directory.path().join("moved/fifo"))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "mkfifo failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let request = Request {
                id: 3,
                revision: 3,
                anchor,
                relative: PathBuf::from("last"),
                kind: NodeKind::File,
                cancelled: AtomicBool::new(false),
            };
            assert_eq!(
                read_contents(
                    &request,
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &mut Some(helpers::Catalog::empty()),
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &helpers::Control::default()
                )
                .unwrap()
                .text
                .as_deref(),
                Some("last")
            );
            let request = Request {
                relative: PathBuf::from("fifo"),
                ..request
            };
            assert!(
                read_contents(
                    &request,
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &mut Some(helpers::Catalog::empty()),
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &helpers::Control::default()
                )
                .is_err()
            );
            request.cancelled.store(true, Ordering::Relaxed);
            assert_eq!(
                read_contents(
                    &request,
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &mut Some(helpers::Catalog::empty()),
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &helpers::Control::default()
                )
                .err()
                .unwrap()
                .kind(),
                io::ErrorKind::Interrupted
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_show_only_the_stored_destination_and_expected_kind_is_enforced() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        std::fs::create_dir_all(".tmp").unwrap();
        let directory = tempfile::tempdir_in(".tmp").unwrap();
        let root = directory.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), "never show this payload").unwrap();
        std::fs::create_dir(root.join("folder")).unwrap();
        std::fs::write(root.join("folder/secret"), "never enumerate this folder").unwrap();
        let anchor = Arc::new(RootAnchor::open(&root).unwrap());
        for (name, raw, expected) in [
            ("file-link", b"file".as_slice(), "file"),
            ("folder-link", b"folder".as_slice(), "folder"),
            ("dangling", b"../../missing".as_slice(), "../../missing"),
            ("absolute", b"/outside/root".as_slice(), "/outside/root"),
            (
                "raw",
                b"a\xff\\x\n\t\x1b".as_slice(),
                "a\\xff\\\\x\\n\\t\\u{1b}",
            ),
        ] {
            symlink(std::ffi::OsStr::from_bytes(raw), root.join(name)).unwrap();
            let request = Request {
                id: 1,
                revision: 1,
                anchor: Arc::clone(&anchor),
                relative: name.into(),
                kind: NodeKind::Symlink,
                cancelled: AtomicBool::new(false),
            };
            let result = read_contents(
                &request,
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &mut Some(helpers::Catalog::empty()),
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &helpers::Control::default(),
            )
            .unwrap();
            assert_eq!(result.kind, "Symlink");
            assert_eq!(result.text.as_deref(), Some(expected));
            assert!(!result.truncated);
            let request = Request {
                kind: NodeKind::File,
                ..request
            };
            assert!(
                read_contents(
                    &request,
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &mut Some(helpers::Catalog::empty()),
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "freebsd",
                        target_os = "dragonfly"
                    ))]
                    &helpers::Control::default()
                )
                .is_err()
            );
        }
        // A vanished/replaced link must never turn into a regular-file contents request.
        let request = Request {
            id: 1,
            revision: 1,
            anchor: Arc::clone(&anchor),
            relative: "file".into(),
            kind: NodeKind::Symlink,
            cancelled: AtomicBool::new(false),
        };
        assert!(
            read_contents(
                &request,
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &mut Some(helpers::Catalog::empty()),
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &helpers::Control::default()
            )
            .is_err()
        );
        symlink("folder", root.join("parent")).unwrap();
        let request = Request {
            relative: "parent/secret".into(),
            kind: NodeKind::File,
            ..request
        };
        assert!(
            read_contents(
                &request,
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &mut Some(helpers::Catalog::empty()),
                #[cfg(any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly"
                ))]
                &helpers::Control::default()
            )
            .is_err()
        );

        let mut preview = FilePreview::new().unwrap();
        preview.request(1, Arc::clone(&anchor), "file".into(), NodeKind::File);
        let old = preview.request.load_full().unwrap();
        preview.request(1, anchor, "folder-link".into(), NodeKind::Symlink);
        assert!(old.cancelled.load(Ordering::Relaxed));
        assert!(preview.matches(1, NodeKind::Symlink));
        assert!(!preview.matches(1, NodeKind::File));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while preview.snapshot().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(preview.snapshot().unwrap().text.as_deref(), Some("folder"));
        preview.shutdown();
        assert!(preview.selected().is_none() && preview.snapshot().is_none());
        preview.request(2, Arc::clone(&old.anchor), "file".into(), NodeKind::File);
        assert!(
            preview.selected().is_none(),
            "stopped preview must not restart"
        );
    }

    #[test]
    fn link_path_escaping_is_reversible_and_bounded() {
        assert_eq!(
            escape_path("é\\literal\n\u{202e}tail".as_bytes()),
            ("é\\\\literal\\n\\u{202e}tail".into(), false)
        );
        assert_eq!(escape_path(b"bad\xff\xfe"), ("bad\\xff\\xfe".into(), false));
        for input in [
            vec![0xff; READ_LIMIT],
            vec![b'\\'; READ_LIMIT],
            vec![b'a'; READ_LIMIT],
        ] {
            let (text, truncated) = escape_path(&input);
            assert!(truncated && text.len() <= TEXT_LIMIT);
            assert!(!text.contains('\n') && !text.contains('\u{1b}'));
        }
    }
}
