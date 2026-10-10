//! Bounded, latest-selection file previews. All filesystem work stays off the UI.
use crate::{backend::RootAnchor, types::NodeId};
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
}

impl FilePreview {
    pub fn new() -> io::Result<Self> {
        let request: Arc<ArcSwapOption<Request>> = Arc::new(ArcSwapOption::empty());
        let result: Arc<ArcSwapOption<FileContents>> = Arc::new(ArcSwapOption::empty());
        let (wake, receiver) = bounded(1);
        let requests = Arc::clone(&request);
        let results = Arc::clone(&result);
        std::thread::Builder::new()
            .name("ldu-preview".into())
            .spawn(move || {
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
                    let mut contents = read_contents(&request).unwrap_or(FileContents {
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
        })
    }

    pub fn selected(&self) -> Option<NodeId> {
        self.request.load().as_ref().map(|request| request.id)
    }

    pub fn request(&mut self, id: NodeId, anchor: Arc<RootAnchor>, relative: PathBuf) {
        if self.selected() == Some(id) {
            return;
        }
        self.clear();
        self.revision = self.revision.wrapping_add(1);
        self.request.store(Some(Arc::new(Request {
            id,
            revision: self.revision,
            anchor,
            relative,
            cancelled: AtomicBool::new(false),
        })));
        let _ = self.wake.try_send(());
    }

    pub fn clear(&mut self) {
        if let Some(previous) = self.request.swap(None) {
            previous.cancelled.store(true, Ordering::Relaxed);
        }
        self.result.store(None);
    }

    pub fn snapshot(&self) -> Option<Arc<FileContents>> {
        let request = self.request.load();
        let revision = request.as_ref()?.revision;
        self.result
            .load_full()
            .filter(|contents| contents.revision == revision)
    }
}

impl Drop for FilePreview {
    fn drop(&mut self) {
        self.clear();
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
fn open_file(request: &Request) -> io::Result<(std::fs::File, u64)> {
    use rustix::fs::{Mode, OFlags, openat};
    cancelled(request)?;
    #[cfg(target_os = "linux")]
    let parent = request
        .anchor
        .open_parent_cancellable(&request.relative, &request.cancelled)?;
    #[cfg(not(target_os = "linux"))]
    let parent = {
        use std::path::{Component, Path};
        let root = request.anchor.root_path();
        let mut fd = rustix::fs::open(
            if root.is_absolute() { "/" } else { "." },
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        for part in root.components() {
            let name = match part {
                Component::Normal(name) => name,
                Component::ParentDir => std::ffi::OsStr::new(".."),
                Component::RootDir | Component::CurDir => continue,
                _ => return Err(io::Error::other("invalid scan root")),
            };
            cancelled(request)?;
            fd = openat(
                &fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
        }
        for part in request
            .relative
            .parent()
            .unwrap_or(Path::new(""))
            .components()
        {
            let Component::Normal(name) = part else {
                return Err(io::Error::other("invalid preview path"));
            };
            cancelled(request)?;
            fd = openat(
                &fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
        }
        fd
    };
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

fn read_contents(request: &Request) -> io::Result<FileContents> {
    let (file, length) = open_file(request)?;
    let (bytes, truncated) = read_prefix(file, length, || cancelled(request))?;
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

fn file_kind(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        "PDF"
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

fn decode(bytes: &[u8], mut truncated: bool) -> FileContents {
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
        if bytes.len() % 2 != 0 && !truncated {
            return fallback();
        }
        let mut units: Vec<_> = bytes
            .chunks_exact(2)
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
                cancelled: AtomicBool::new(false),
            };
            assert!(read_contents(&request).is_err(), "{path}");
        }
        let mut preview = FilePreview::new().unwrap();
        preview.request(1, Arc::clone(&anchor), PathBuf::from("first"));
        preview.clear();
        assert!(preview.selected().is_none() && preview.snapshot().is_none());
        preview.request(2, Arc::clone(&anchor), PathBuf::from("last"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while preview.snapshot().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(preview.snapshot().unwrap().text.as_deref(), Some("last"));
        #[cfg(target_os = "linux")]
        {
            std::fs::rename(&root, directory.path().join("moved")).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("last"), "replacement").unwrap();
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                directory.path().join("moved/fifo"),
                rustix::fs::Mode::RUSR,
            )
            .unwrap();
            let request = Request {
                id: 3,
                revision: 3,
                anchor,
                relative: PathBuf::from("last"),
                cancelled: AtomicBool::new(false),
            };
            assert_eq!(
                read_contents(&request).unwrap().text.as_deref(),
                Some("last")
            );
            let request = Request {
                relative: PathBuf::from("fifo"),
                ..request
            };
            assert!(read_contents(&request).is_err());
            request.cancelled.store(true, Ordering::Relaxed);
            assert_eq!(
                read_contents(&request).err().unwrap().kind(),
                io::ErrorKind::Interrupted
            );
        }
    }
}
