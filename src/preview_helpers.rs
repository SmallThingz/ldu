//! Fixed, read-only system previews. Runs only on the latest-preview worker.
//! Requires non-reaping child status checks so process-group ownership survives
//! until descendants are stopped. Unsupported platforms retain file details.
use super::{FileContents, READ_LIMIT, archive_magic, text_contents};
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const TIME_LIMIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Default)]
struct ProcessState {
    shutdown: bool,
    active: Option<u32>,
}

#[derive(Default)]
pub(super) struct Control {
    state: Mutex<ProcessState>,
}

impl Control {
    fn state(&self) -> MutexGuard<'_, ProcessState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(super) fn is_shutdown(&self) -> bool {
        self.state().shutdown
    }

    pub(super) fn cancel(&self) {
        let state = self.state();
        if let Some(pid) = state.active {
            kill_group(pid);
        }
    }

    pub(super) fn shutdown(&self) {
        let mut state = self.state();
        state.shutdown = true;
        if let Some(pid) = state.active {
            kill_group(pid);
        }
    }

    fn register(&self, pid: u32) -> bool {
        let mut state = self.state();
        if state.shutdown || state.active.is_some() {
            return false;
        }
        state.active = Some(pid);
        true
    }

    fn retire(&self, pid: u32) {
        let mut state = self.state();
        // Kill BEFORE forgetting the PID. Otherwise shutdown could miss a live
        // helper between unregistering and worker-local cleanup.
        kill_group(pid);
        if state.active == Some(pid) {
            state.active = None;
        }
        // The worker reaps only after dropping this lock. A concurrent shutdown
        // can therefore never signal a PID after its ownership has been released.
    }
}

fn kill_group(pid: u32) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = Pid::from_raw(pid as i32) {
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    BsdTar,
    Tar,
    ZipInfo,
    Unzip,
    SevenZip,
}

pub(super) struct Catalog {
    tools: Vec<(Tool, PathBuf)>,
    path: OsString,
}

impl Catalog {
    fn discover() -> Self {
        // Cache absence too. Do not execute programs or consult PATH on redraws.
        let paths: Vec<_> = std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|path| path.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        let mut tools = Vec::with_capacity(5);
        for (tool, names) in [
            (Tool::BsdTar, &["bsdtar"][..]),
            (Tool::Tar, &["tar"][..]),
            (Tool::ZipInfo, &["zipinfo"][..]),
            (Tool::Unzip, &["unzip"][..]),
            (Tool::SevenZip, &["7zz", "7z"][..]),
        ] {
            let found = names.iter().find_map(|name| {
                paths.iter().find_map(|directory| {
                    let candidate = directory.join(name);
                    let metadata = candidate.metadata().ok()?;
                    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                        .then_some(candidate)
                })
            });
            if let Some(path) = found {
                tools.push((tool, path));
            }
        }
        Self {
            tools,
            path: std::env::join_paths(paths).unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self {
            tools: Vec::new(),
            path: OsString::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Zip,
    Tar,
    Gzip,
    Bzip,
    Xz,
    Zstd,
    Archive,
}

fn format(bytes: &[u8], path: &Path) -> Option<Format> {
    if bytes.starts_with(b"%PDF-") {
        // Recognized documents remain file details, even with an archive extension.
        return None;
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Some(Format::Zip);
    }
    if bytes.starts_with(b"\x1f\x8b") {
        return Some(Format::Gzip);
    }
    if bytes.starts_with(b"BZh") {
        return Some(Format::Bzip);
    }
    if bytes.starts_with(b"\xfd7zXZ\0") {
        return Some(Format::Xz);
    }
    if bytes.starts_with(b"\x28\xb5\x2f\xfd") {
        return Some(Format::Zstd);
    }
    if bytes.get(257..262) == Some(b"ustar") {
        return Some(Format::Tar);
    }
    if archive_magic(bytes) {
        return Some(Format::Archive);
    }
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("tar") {
        return Some(Format::Tar);
    }
    if ["zip", "jar", "apk", "epub", "docx", "xlsx", "pptx"]
        .iter()
        .any(|value| extension.eq_ignore_ascii_case(value))
    {
        return Some(Format::Zip);
    }
    if [
        "7z", "rar", "cpio", "a", "ar", "deb", "iso", "cab", "xar", "z", "lz", "lzma",
    ]
    .iter()
    .any(|value| extension.eq_ignore_ascii_case(value))
    {
        return Some(Format::Archive);
    }
    None
}

fn arguments(tool: Tool, format: Format) -> Option<&'static [&'static str]> {
    match (tool, format) {
        (Tool::BsdTar, _) => Some(&[
            "--passphrase",
            "ldu-preview-no-password",
            "-tf",
            "/dev/fd/0",
        ]),
        (Tool::Tar, Format::Zip | Format::Archive) => Some(&[
            "--passphrase",
            "ldu-preview-no-password",
            "-tf",
            "/dev/fd/0",
        ]),
        (Tool::Tar, Format::Tar) => Some(&["-tf", "/dev/fd/0"]),
        (Tool::Tar, Format::Gzip) => Some(&["-tzf", "/dev/fd/0"]),
        (Tool::Tar, Format::Bzip) => Some(&["-tjf", "/dev/fd/0"]),
        (Tool::Tar, Format::Xz) => Some(&["-tJf", "/dev/fd/0"]),
        (Tool::Tar, Format::Zstd) => Some(&["--zstd", "-tf", "/dev/fd/0"]),
        (Tool::ZipInfo, Format::Zip) => Some(&["-1", "/dev/fd/0"]),
        (Tool::Unzip, Format::Zip) => Some(&["-Z1", "/dev/fd/0"]),
        (Tool::SevenZip, _) => Some(&[
            "l",
            "-ba",
            "-bd",
            "-mmt=1",
            "-sccUTF-8",
            "-p-",
            "--",
            "/dev/fd/0",
        ]),
        _ => None,
    }
}

pub(super) fn preview(
    file: &mut File,
    bytes: &[u8],
    relative: &Path,
    cancel: &AtomicBool,
    catalog: &mut Option<Catalog>,
    control: &Control,
) -> io::Result<Option<FileContents>> {
    let Some(format) = format(bytes, relative) else {
        return Ok(None);
    };
    let catalog = catalog.get_or_insert_with(Catalog::discover);
    let deadline = Instant::now() + TIME_LIMIT;
    for (tool, executable) in &catalog.tools {
        let Some(args) = arguments(*tool, format) else {
            continue;
        };
        check(cancel)?;
        if Instant::now() >= deadline {
            break;
        }
        // Clones share the underlying offset. Rewind after sniffing and before every fallback.
        file.rewind()?;
        match run(
            executable,
            args,
            file.try_clone()?,
            &catalog.path,
            cancel,
            deadline,
            control,
        ) {
            Ok(Some((bytes, truncated))) => {
                let mut contents = output_text(&bytes, truncated);
                contents.kind = "Archive";
                if contents.text.as_deref() == Some("") {
                    contents.text = Some("Empty archive.".into());
                }
                return Ok(Some(contents));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
            // Missing, unsupported, corrupt and encrypted formats retain normal details.
            Err(_) | Ok(None) => {}
        }
    }
    check(cancel)?;
    Ok(None)
}

fn check(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "preview cancelled",
        ))
    } else {
        Ok(())
    }
}

struct OwnedChild<'a> {
    child: Child,
    control: &'a Control,
}

impl OwnedChild<'_> {
    fn exited(&self) -> io::Result<Option<bool>> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let pid = Pid::from_raw(self.child.id() as i32)
            .ok_or_else(|| io::Error::other("invalid helper process"))?;
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => Ok(status.map(|status| status.exit_status() == Some(0))),
            Err(rustix::io::Errno::INTR) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for OwnedChild<'_> {
    fn drop(&mut self) {
        self.control.retire(self.child.id());
        let _ = self.child.kill();
        // Waiting stays on the preview worker, outside the registry lock.
        let _ = self.child.wait();
    }
}

fn launch_command(
    executable: &Path,
    arguments: &[&str],
    input: File,
    path: &std::ffi::OsStr,
) -> Command {
    // This constant shell program is only a one-shot start gate. Neither the
    // executable, its arguments, nor file bytes are interpreted as shell code.
    // Before registration, parent exit closes the pipe and read fails: no utility
    // can start in the otherwise unobservable fork/exec interval of spawn().
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("IFS= read -r ready && [ \"$ready\" = x ] && exec \"$@\" <&2 2>/dev/null")
        .arg("ldu-preview-gate")
        .arg(executable)
        .args(arguments)
        .env_clear()
        .env("PATH", path)
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The gate reads only fd 0. Its fixed redirections make this already-open
        // regular file the utility's stdin before silencing utility stderr.
        .stderr(Stdio::from(input))
        .process_group(0);
    command
}

fn run(
    executable: &Path,
    arguments: &[&str],
    input: File,
    path: &std::ffi::OsStr,
    cancel: &AtomicBool,
    deadline: Instant,
    control: &Control,
) -> io::Result<Option<(Vec<u8>, bool)>> {
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        fs::{OFlags, fcntl_getfl, fcntl_setfl},
    };
    check(cancel)?;
    if control.is_shutdown() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "preview stopped",
        ));
    }
    let child = launch_command(executable, arguments, input, path).spawn()?;
    let mut child = OwnedChild { child, control };
    if !control.register(child.child.id()) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "preview stopped",
        ));
    }
    check(cancel)?;
    // Registration precedes permission to exec the utility. A concurrent stop
    // either rejects registration or signals this still-owned process group.
    let mut start = child
        .child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing helper start gate"))?;
    start.write_all(b"x\n")?;
    drop(start);
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing helper output"))?;
    let flags = fcntl_getfl(&stdout)?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK)?;
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0; 4096];
    let mut eof = false;
    loop {
        check(cancel)?;
        if Instant::now() >= deadline {
            return Ok((!bytes.is_empty()).then_some((bytes, true)));
        }
        if !eof {
            let limit = chunk.len().min(READ_LIMIT - bytes.len());
            match stdout.read(&mut chunk[..limit]) {
                Ok(0) => eof = true,
                Ok(count) => {
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.len() == READ_LIMIT {
                        return Ok(Some((bytes, true)));
                    }
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if eof {
            if let Some(success) = child.exited()? {
                return Ok(success.then_some((bytes, false)));
            }
            // HUP stays ready forever. An empty poll sleeps without a busy loop.
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: wait.as_nanos() as _,
            };
            match poll(&mut [], Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: wait.as_nanos() as _,
            };
            match poll(&mut [PollFd::new(&stdout, PollFlags::IN)], Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn output_text(mut bytes: &[u8], truncated: bool) -> FileContents {
    use std::fmt::Write;
    // Helpers produce text, so magic-looking member names are never decoded as files.
    // Preserve invalid output bytes explicitly instead of dropping a whole listing.
    let mut text = String::with_capacity(bytes.len());
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                text.push_str(valid);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text.push_str(std::str::from_utf8(&bytes[..valid]).unwrap());
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                for byte in &bytes[valid..valid + invalid] {
                    let _ = write!(text, "\\x{byte:02x}");
                }
                bytes = &bytes[valid + invalid..];
            }
        }
    }
    text_contents(&text, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(program: &str) -> (tempfile::TempDir, PathBuf, File) {
        std::fs::create_dir_all(".tmp").unwrap();
        let directory = tempfile::tempdir_in(".tmp").unwrap();
        let executable = directory.path().join("helper");
        std::fs::write(&executable, format!("#!/bin/sh\n{program}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = executable.canonicalize().unwrap();
        let input = directory.path().join("input");
        std::fs::write(&input, b"PK\x03\x04anchored\n").unwrap();
        let input = File::open(input).unwrap();
        (directory, executable, input)
    }

    #[test]
    fn fixed_helper_plans_and_output_do_not_depend_on_file_magic_or_names() {
        for (magic, expected) in [
            (b"PK\x03\x04".as_slice(), Format::Zip),
            (b"\x1f\x8b", Format::Gzip),
            (b"BZh9", Format::Bzip),
            (b"\xfd7zXZ\0", Format::Xz),
            (b"7z\xbc\xaf\x27\x1c", Format::Archive),
            (b"Rar!\x1a\x07", Format::Archive),
        ] {
            assert_eq!(
                format(magic, Path::new("--a malicious name")),
                Some(expected)
            );
        }
        assert_eq!(format(b"", Path::new("old.TAR")), Some(Format::Tar));
        assert_eq!(format(b"text", Path::new("notes.txt")), None);
        let args = arguments(Tool::Tar, Format::Zip).unwrap();
        assert!(args.contains(&"--passphrase") && args.contains(&"/dev/fd/0"));
        let output = output_text(b"%PDF-member\nGIF89a\nraw\xff\n\x1b[31m\0", false);
        assert_eq!(
            output.text.as_deref(),
            Some("%PDF-member\nGIF89a\nraw\\xff\n\\u{1b}[31m\\u{0}")
        );
        let output = output_text(&b"x\n".repeat(READ_LIMIT), false);
        assert!(output.truncated);
        assert!(output.text.unwrap().lines().count() <= super::super::LINE_LIMIT);
    }

    #[test]
    fn short_reads_preserve_extensionless_tar_helper_detection() {
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
        // A valid empty-file ustar header followed by its two end blocks.
        // Its early NULs precede the signature, independently of chunk size.
        let mut archive = vec![0; 1536];
        archive[..4].copy_from_slice(b"leaf");
        archive[100..108].copy_from_slice(b"0000644\0");
        archive[108..116].copy_from_slice(b"0000000\0");
        archive[116..124].copy_from_slice(b"0000000\0");
        archive[124..136].copy_from_slice(b"00000000000\0");
        archive[136..148].copy_from_slice(b"00000000000\0");
        archive[148..156].fill(b' ');
        archive[156] = b'0';
        archive[257..263].copy_from_slice(b"ustar\0");
        archive[263..265].copy_from_slice(b"00");
        let checksum: u32 = archive[..512].iter().map(|&byte| u32::from(byte)).sum();
        archive[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        for hint in [0, 1, u64::MAX] {
            for chunk in [1, 7, 31, 32, 128, 257, 261, 262, 4096] {
                let mut reader = Counted {
                    bytes: &archive,
                    chunk,
                    reads: 0,
                };
                let (bytes, truncated) =
                    super::super::read_prefix(&mut reader, hint, || Ok(())).unwrap();
                assert_eq!(
                    format(&bytes, Path::new("extensionless")),
                    Some(Format::Tar),
                    "hint={hint}, chunk={chunk}"
                );
                assert!(!truncated);
                let reads = super::super::MAGIC_LIMIT.div_ceil(chunk);
                assert_eq!(reader.reads, reads);
                assert_eq!(bytes.len(), archive.len().min(reads * chunk));
            }
        }
    }

    #[test]
    fn pdf_files_skip_helper_discovery_and_archive_extension_fallbacks() {
        std::fs::create_dir_all(".tmp").unwrap();
        let mut input = tempfile::tempfile_in(".tmp").unwrap();
        let bytes = b"%PDF-1.7\nPDF contents must not be shown\n";
        input.write_all(bytes).unwrap();
        let cancel = AtomicBool::new(false);
        let mut catalog = None;
        for name in ["document.pdf", "renamed.zip", "renamed.tar", "renamed.7z"] {
            assert!(
                preview(
                    &mut input,
                    bytes,
                    Path::new(name),
                    &cancel,
                    &mut catalog,
                    &Control::default(),
                )
                .unwrap()
                .is_none()
            );
            assert!(catalog.is_none());
        }
    }

    #[test]
    fn missing_helpers_skip_contents_and_fallbacks_rewind_the_anchored_file() {
        let (directory, failing, mut input) =
            fixture("/bin/cat >/dev/null\nprintf 'wrong helper output'\nexit 2");
        let success = directory.path().join("success");
        std::fs::write(&success, "#!/bin/sh\n/bin/cat\n").unwrap();
        std::fs::set_permissions(&success, std::fs::Permissions::from_mode(0o700)).unwrap();
        let success = success.canonicalize().unwrap();
        let cancel = AtomicBool::new(false);
        assert!(
            preview(
                &mut input,
                b"PK\x03\x04",
                Path::new("archive.zip"),
                &cancel,
                &mut Some(Catalog::empty()),
                &Control::default()
            )
            .unwrap()
            .is_none()
        );
        let mut sniffed = [0; 4];
        input.read_exact(&mut sniffed).unwrap();
        let catalog = Catalog {
            tools: vec![(Tool::BsdTar, failing), (Tool::Unzip, success)],
            path: OsString::new(),
        };
        let result = preview(
            &mut input,
            &sniffed,
            Path::new("--user name.zip"),
            &cancel,
            &mut Some(catalog),
            &Control::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.kind, "Archive");
        assert_eq!(result.text.as_deref(), Some("PK\\u{3}\\u{4}anchored\n"));
        assert!(!result.truncated);
    }

    #[test]
    fn helper_descriptor_path_reads_the_opened_file_after_path_replacement() {
        let (directory, executable, input) = fixture("/bin/cat \"$1\"");
        let original = directory.path().join("input");
        std::fs::rename(&original, directory.path().join("moved")).unwrap();
        std::fs::write(&original, b"replacement must not be read").unwrap();
        let result = run(
            &executable,
            &["/dev/fd/0"],
            input,
            std::ffi::OsStr::new(""),
            &AtomicBool::new(false),
            Instant::now() + TIME_LIMIT,
            &Control::default(),
        )
        .unwrap();
        // BSD systems may omit the fd filesystem. Such helpers must fail closed
        // and leave archive details available, never reopen the display path.
        if Path::new("/dev/fd").is_dir() {
            let (bytes, truncated) = result.expect("descriptor path must be usable");
            assert_eq!(bytes, b"PK\x03\x04anchored\n");
            assert!(!truncated);
        } else {
            assert!(result.is_none());
        }
    }

    #[test]
    fn helper_output_and_runtime_are_bounded_and_descendants_are_retired() {
        let (_directory, executable, input) =
            fixture("while :; do printf '1234567890123456789012345678901234567890'; done");
        let cancel = AtomicBool::new(false);
        let (bytes, truncated) = run(
            &executable,
            &[],
            input,
            std::ffi::OsStr::new(""),
            &cancel,
            Instant::now() + TIME_LIMIT,
            &Control::default(),
        )
        .unwrap()
        .unwrap();
        assert!(truncated);
        assert_eq!(bytes.len(), READ_LIMIT);

        // The leader exits first; its descendant holds stdout open. Group cleanup must
        // occur before reaping that leader, keeping the group identity owned.
        let (_directory, executable, input) =
            fixture("/bin/sleep 10 &\nprintf '%s\\n' \"$!\"\nexit 0");
        let start = Instant::now();
        let (bytes, truncated) = run(
            &executable,
            &[],
            input,
            std::ffi::OsStr::new(""),
            &cancel,
            start + Duration::from_millis(300),
            &Control::default(),
        )
        .unwrap()
        .unwrap();
        assert!(truncated && start.elapsed() < Duration::from_secs(3));
        #[cfg(target_os = "linux")]
        {
            let pid: u32 = std::str::from_utf8(&bytes).unwrap().trim().parse().unwrap();
            let stat = format!("/proc/{pid}/stat");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let running = std::fs::read_to_string(&stat).ok().is_some_and(|stat| {
                    let state = stat.rsplit_once(") ").unwrap().1.chars().next().unwrap();
                    state != 'Z' && state != 'X'
                });
                if !running {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned descendant remained running"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(not(target_os = "linux"))]
        assert!(!bytes.is_empty());
    }

    #[test]
    fn helper_cancellation_interrupts_closed_stdout_wait_without_busy_polling() {
        let (_directory, executable, input) = fixture("exec 1>&-\n/bin/sleep 10");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let notify = std::sync::Arc::clone(&cancel);
        let interrupter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            notify.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let result = run(
            &executable,
            &[],
            input,
            std::ffi::OsStr::new(""),
            &cancel,
            start + TIME_LIMIT,
            &Control::default(),
        );
        interrupter.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    fn wait_exit(child: &OwnedChild<'_>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(success) = child.exited().unwrap() {
                return success;
            }
            assert!(Instant::now() < deadline, "owned helper did not stop");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn unregistered_gate_exits_without_executing_when_its_parent_pipe_closes() {
        let (_directory, executable, input) = fixture("printf 'must not execute'");
        let control = Control::default();
        let child = launch_command(&executable, &[], input, std::ffi::OsStr::new(""))
            .spawn()
            .unwrap();
        let mut child = OwnedChild {
            child,
            control: &control,
        };
        let mut stdout = child.child.stdout.take().unwrap();
        // Models process exit before Command::spawn exposes the gate's PID.
        drop(child.child.stdin.take());
        assert!(!wait_exit(&child));
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).unwrap();
        assert!(output.is_empty());
        assert!(control.state().active.is_none());
    }

    #[test]
    fn shutdown_before_registration_rejects_the_gate_and_forbids_restarts() {
        let (_directory, executable, input) = fixture("printf 'must not execute'");
        let control = Control::default();
        let child = launch_command(&executable, &[], input, std::ffi::OsStr::new(""))
            .spawn()
            .unwrap();
        let mut child = OwnedChild {
            child,
            control: &control,
        };
        let mut stdout = child.child.stdout.take().unwrap();
        control.shutdown();
        assert!(!control.register(child.child.id()));
        drop(child);
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).unwrap();
        assert!(output.is_empty() && control.is_shutdown());
        assert!(control.state().active.is_none());
    }

    #[test]
    fn synchronous_cancellation_retains_group_identity_until_kill_then_reap() {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        for shutdown in [false, true] {
            let (_directory, executable, input) = fixture("printf 'ready'\nexec /bin/sleep 30");
            let control = Control::default();
            let child = launch_command(&executable, &[], input, std::ffi::OsStr::new(""))
                .spawn()
                .unwrap();
            let mut child = OwnedChild {
                child,
                control: &control,
            };
            let pid = child.child.id();
            assert!(control.register(pid));
            let mut start = child.child.stdin.take().unwrap();
            start.write_all(b"x\n").unwrap();
            drop(start);
            let mut stdout = child.child.stdout.take().unwrap();
            fcntl_setfl(&stdout, fcntl_getfl(&stdout).unwrap() | OFlags::NONBLOCK).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut output = [0; 5];
            let mut received = 0;
            while received < output.len() {
                match stdout.read(&mut output[received..]) {
                    Ok(0) => panic!("helper exited before readiness"),
                    Ok(count) => received += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        assert!(Instant::now() < deadline, "helper did not start");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("helper readiness failed: {error}"),
                }
            }
            assert_eq!(&output, b"ready");
            let start = Instant::now();
            if shutdown {
                control.shutdown();
            } else {
                control.cancel();
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(!wait_exit(&child));
            // waitid(NOWAIT) has not reaped the leader, and the registry still
            // retains ownership while UI cancellation can signal it again.
            assert_eq!(control.state().active, Some(pid));
            control.cancel();
            drop(child);
            assert!(control.state().active.is_none());
            assert_eq!(control.is_shutdown(), shutdown);
        }
    }
}
