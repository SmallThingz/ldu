#![cfg(unix)]

use ldu::{
    backend::Backend,
    model::Model,
    scan::{self, ScanOptions},
    types::{NodeId, NodeKind, ROOT, ScanEvent, Sizes},
};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File, Permissions},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn fixture() -> tempfile::TempDir {
    fs::create_dir_all(".tmp").unwrap();
    tempfile::Builder::new()
        .prefix("ldu-fs-")
        .tempdir_in(".tmp")
        .unwrap()
}

/// Independent filesystem oracle: no-follow metadata, global inode deduplication.
fn reference(path: &Path, seen: &mut HashSet<(u64, u64)>) -> (Sizes, u64) {
    let metadata = fs::symlink_metadata(path).unwrap();
    let duplicate = metadata.is_file()
        && metadata.nlink() > 1
        && !seen.insert((metadata.dev(), metadata.ino()));
    let mut sizes = if duplicate {
        Sizes::default()
    } else {
        Sizes {
            apparent: metadata.len(),
            allocated: metadata.blocks() * 512,
        }
    };
    let mut descendants = 0;
    if metadata.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            let (child_sizes, child_descendants) = reference(&entry.unwrap().path(), seen);
            sizes.apparent += child_sizes.apparent;
            sizes.allocated += child_sizes.allocated;
            descendants += 1 + child_descendants;
        }
    }
    (sizes, descendants)
}

fn scan_model(root: &Path, backend: Backend) -> Model {
    let handle = scan::spawn(
        root.to_owned(),
        ScanOptions {
            threads: 4,
            batch_size: 3,
            buffer_size: 4096,
            backend,
        },
    );
    let mut model = Model::new(root.to_owned());
    let mut delivered = Sizes::default();
    let mut directories = HashSet::from([ROOT]);
    let mut done = HashSet::new();
    let mut root_received = false;
    let mut next_id = ROOT + 1;
    loop {
        let event = handle
            .receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        let finished = matches!(event, ScanEvent::Finished { .. });
        match &event {
            ScanEvent::RootInfo { sizes, .. } => {
                assert!(!root_received, "root metadata must be delivered once");
                root_received = true;
                delivered.apparent += sizes.apparent;
                delivered.allocated += sizes.allocated;
            }
            ScanEvent::Entries { parent, entries } => {
                assert!(root_received, "root metadata must precede entries");
                assert!(
                    directories.contains(parent),
                    "entry parent must already exist"
                );
                assert!(
                    !done.contains(parent),
                    "entries cannot follow their directory's end"
                );
                for entry in entries {
                    assert_eq!(entry.id, next_id, "entry IDs must be contiguous and unique");
                    next_id += 1;
                    if !entry.hardlink_duplicate {
                        delivered.apparent += entry.sizes.apparent;
                        delivered.allocated += entry.sizes.allocated;
                    }
                    if entry.kind == NodeKind::Directory {
                        assert!(directories.insert(entry.id));
                    }
                }
            }
            ScanEvent::DirectoryDone { id, .. } => {
                assert!(
                    root_received,
                    "root metadata must precede directory completion"
                );
                assert!(
                    directories.contains(id),
                    "completed directory must already exist"
                );
                assert!(done.insert(*id), "a directory must finish exactly once");
            }
            ScanEvent::Finished { .. } => {
                assert!(root_received);
                assert_eq!(done, directories, "all discovered directories must finish");
            }
        }
        let _ = model.apply(event);
        assert_eq!(
            model.node(ROOT).sizes,
            delivered,
            "live totals must conserve delivered bytes"
        );
        assert_eq!(model.node(ROOT).items, u64::from(next_id - 1));
        assert_eq!(model.len() as NodeId, next_id);
        for id in directories.iter().copied() {
            if model.node(id).complete {
                assert!(done.contains(&id));
                assert!(model.children(id).all(|child| model.node(child).complete));
            }
        }
        if finished {
            break;
        }
    }
    assert!(model.finished && !model.cancelled);
    assert!(model.node(ROOT).complete);
    assert_eq!(model.node(ROOT).error_count, 0, "{:?}", model.last_error);
    model
}

#[test]
fn streamed_scan_conserves_inode_totals_preserves_names_and_never_follows_symlinks() {
    let work = fixture();
    let root = work.path();
    fs::create_dir(root.join("a-fast")).unwrap();
    fs::create_dir(root.join("a-fast/deep")).unwrap();
    fs::write(root.join("a-fast/deep/leaf"), b"deep").unwrap();
    fs::create_dir(root.join("z-wide")).unwrap();
    for n in 0..400 {
        fs::write(root.join("z-wide").join(format!("item-{n:04}")), b"x").unwrap();
    }
    let sparse = root.join("sparse");
    File::create(&sparse)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    fs::hard_link(&sparse, root.join("a-fast/alias")).unwrap();
    symlink(".", root.join("cycle")).unwrap();
    symlink("missing-target", root.join("dangling")).unwrap();
    let raw_name = OsString::from_vec(b"raw-\xff".to_vec());
    fs::write(root.join(&raw_name), b"non UTF-8").unwrap();
    let expected = reference(root, &mut HashSet::new());
    let metadata = fs::symlink_metadata(&sparse).unwrap();
    assert!(
        metadata.len() > metadata.blocks() * 512,
        "fixture must actually be sparse"
    );
    for backend in [Backend::Std, Backend::Auto] {
        let model = scan_model(root, backend);
        assert_eq!((model.node(ROOT).sizes, model.node(ROOT).items), expected);
        assert_eq!(model.len() as u64, expected.1 + 1);
        assert!(
            model
                .children(ROOT)
                .any(|id| model.node(id).name.as_ref() == raw_name.as_os_str())
        );
        let duplicates = (1..model.len() as NodeId)
            .filter(|&id| model.node(id).hardlink_duplicate)
            .count();
        assert_eq!(
            duplicates, 1,
            "one alias is visible but contributes no duplicate inode bytes"
        );
        for id in 1..model.len() as NodeId {
            if model.node(id).kind == NodeKind::Symlink {
                assert_eq!(model.node(id).items, 0);
                assert_eq!(model.children(id).count(), 0);
            }
        }
    }
}

/// Build and charge deep paths through retained descriptors, including on systems
/// whose PATH_MAX is shorter than the logical fixture path.
fn deep_fixture(root: &Path, depth: usize, padding: &str, payload: &[u8]) -> (PathBuf, Sizes) {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};
    use std::io::Write;

    let mut parent = File::open(root).unwrap();
    let mut relative = PathBuf::new();
    let mut expected = Sizes::default();
    let charge = |metadata: fs::Metadata| Sizes {
        apparent: metadata.len(),
        allocated: metadata.blocks() * 512,
    };
    for level in 0..depth {
        let name = format!("d-{level:03}{padding}");
        mkdirat(&parent, name.as_str(), Mode::RWXU).unwrap();
        let own = charge(parent.metadata().unwrap());
        expected.apparent += own.apparent;
        expected.allocated += own.allocated;
        parent = File::from(
            openat(
                &parent,
                name.as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap(),
        );
        relative.push(name);
    }
    let mut leaf = File::from(
        openat(
            &parent,
            "leaf",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap(),
    );
    leaf.write_all(payload).unwrap();
    for own in [
        charge(parent.metadata().unwrap()),
        charge(leaf.metadata().unwrap()),
    ] {
        expected.apparent += own.apparent;
        expected.allocated += own.allocated;
    }
    (relative.join("leaf"), expected)
}

#[test]
fn scanner_traverses_deep_relative_paths_beyond_the_kernel_pathname_limit() {
    let work = fixture();
    let root = work.path();
    let (relative, expected) = deep_fixture(root, 96, &"x".repeat(40), b"deep scanner payload");
    assert!(relative.as_os_str().as_encoded_bytes().len() > 4096);
    let leaf_path = root.join(relative);
    for backend in [Backend::Auto, Backend::Std, Backend::Native] {
        let model = scan_model(root, backend);
        assert_eq!(model.node(ROOT).sizes, expected);
        assert_eq!(model.node(ROOT).items, 97);
        let leaf = (1..model.len() as NodeId)
            .find(|&id| model.node(id).name.as_ref() == "leaf")
            .unwrap();
        assert_eq!(model.path(leaf), leaf_path);
        assert_eq!(model.node(leaf).sizes.apparent, 20);
    }
}

struct RestorePermissions(PathBuf, Permissions);
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, self.1.clone());
    }
}

#[test]
fn unreadable_directory_finishes_with_observable_error_and_partial_total() {
    let work = fixture();
    let root = work.path();
    let blocked = root.join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("secret"), b"hidden").unwrap();
    fs::write(root.join("visible"), b"visible").unwrap();
    let _restore = RestorePermissions(
        blocked.clone(),
        fs::metadata(&blocked).unwrap().permissions(),
    );
    fs::set_permissions(&blocked, Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        eprintln!("permission-denied fixture skipped: effective user can read mode-000 directory");
        return;
    }
    let handle = scan::spawn(
        root.to_owned(),
        ScanOptions {
            backend: Backend::Std,
            ..Default::default()
        },
    );
    let mut model = Model::new(root.to_owned());
    while !model.finished {
        let _ = model.apply(
            handle
                .receiver
                .recv_timeout(Duration::from_secs(15))
                .unwrap(),
        );
    }
    assert!(!model.cancelled && model.node(ROOT).complete);
    assert_eq!(model.node(ROOT).error_count, 1);
    assert_eq!(
        model.node(ROOT).items,
        2,
        "the unreadable directory remains visible"
    );
    let output = export_scan(root);
    assert_eq!(output.status.code(), Some(2));
    let document = ncdu_document(&output);
    let records = ncdu_records(&document[3]);
    assert_eq!(ncdu_totals(&records), model.node(ROOT).sizes);
    assert!(
        records
            .iter()
            .any(|record| record["name"] == "blocked" && record["read_error"] == true)
    );
    assert!(!records.iter().any(|record| record["name"] == "secret"));
    let blocked_id = model
        .children(ROOT)
        .find(|&id| model.node(id).name.as_ref() == "blocked")
        .unwrap();
    assert!(model.node(blocked_id).complete);
    assert_eq!(model.node(blocked_id).error_count, 1);
    let visible = fs::metadata(root.join("visible")).unwrap();
    let blocked_metadata = fs::metadata(&blocked).unwrap();
    let root_metadata = fs::metadata(root).unwrap();
    assert_eq!(
        model.node(ROOT).sizes,
        Sizes {
            apparent: root_metadata.len() + blocked_metadata.len() + visible.len(),
            allocated: (root_metadata.blocks() + blocked_metadata.blocks() + visible.blocks())
                * 512,
        }
    );
}

fn ldu_command() -> Command {
    let binary =
        std::env::var_os("LDU_TEST_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_ldu").into());
    let mut command = Command::new(binary);
    command.env_remove("XDG_CONFIG_HOME").env_remove("HOME");
    command
}

#[test]
fn cli_error_with_disconnected_stderr_keeps_failure_exit_status() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let result = ldu_command()
        .args(["--threads", "0"])
        .stderr(Stdio::from(OwnedFd::from(writer)))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
}

fn export_scan(path: &Path) -> std::process::Output {
    ldu_command()
        .args(["-o", "-", "--threads", "2"])
        .arg(path)
        .output()
        .unwrap()
}

fn export_scan_from(directory: &Path, path: &Path) -> std::process::Output {
    // Resolve before changing cwd, including a relative LDU_TEST_BIN override.
    let original = ldu_command();
    let binary = std::path::absolute(Path::new(original.get_program())).unwrap();
    Command::new(binary)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .current_dir(directory)
        .args(["-o", "-", "--threads", "2"])
        .arg(path)
        .output()
        .unwrap()
}

fn ncdu_document(output: &std::process::Output) -> serde_json::Value {
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let fields = document.as_array().expect("ncdu export must be an array");
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0], 1);
    assert_eq!(fields[1], 2);
    assert_eq!(fields[2]["progname"], "ldu");
    assert_eq!(fields[2]["progver"], env!("CARGO_PKG_VERSION"));
    assert!(fields[2]["timestamp"].as_u64().is_some());
    assert!(
        fields[3].is_array(),
        "ncdu imports require a directory root"
    );
    document
}

fn ncdu_records(tree: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut pending = vec![tree];
    let mut records = Vec::new();
    while let Some(entry) = pending.pop() {
        if let Some(directory) = entry.as_array() {
            let (record, children) = directory.split_first().expect("directory needs metadata");
            assert!(record.is_object());
            records.push(record);
            pending.extend(children);
        } else {
            assert!(entry.is_object());
            records.push(entry);
        }
    }
    records
}

fn ncdu_totals(records: &[&serde_json::Value]) -> Sizes {
    records.iter().fold(Sizes::default(), |mut total, record| {
        total.apparent += record["asize"].as_u64().unwrap_or(0);
        total.allocated += record["dsize"].as_u64().unwrap_or(0);
        total
    })
}

#[test]
fn headless_export_exports_the_full_ncdu_tree_with_counted_own_sizes() {
    let work = fixture();
    let root = work.path();
    fs::create_dir_all(root.join("branch/nested")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::write(root.join("branch/nested/leaf"), b"nested payload").unwrap();
    let unusual = "quoted-\"-backslash-\\-newline-\n-snowman-☃";
    fs::write(root.join(unusual), b"escaped name").unwrap();
    let sparse = root.join("sparse");
    File::create(&sparse).unwrap().set_len(1 << 20).unwrap();
    fs::hard_link(&sparse, root.join("branch/alias")).unwrap();
    symlink("branch", root.join("directory-link")).unwrap();
    symlink("branch/nested/leaf", root.join("file-link")).unwrap();
    let expected = reference(root, &mut HashSet::new());
    let output = export_scan(root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = ncdu_document(&output);
    let tree = &document[3];
    let records = ncdu_records(tree);
    assert_eq!(records.len() as u64, expected.1 + 1);
    assert_eq!(ncdu_totals(&records), expected.0);
    assert!(records.iter().all(|record| record["read_error"] != true));
    let root_metadata = fs::symlink_metadata(root).unwrap();
    assert_eq!(tree[0]["asize"], root_metadata.len());
    assert_eq!(tree[0]["dsize"], root_metadata.blocks() * 512);
    let branch = tree
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .find(|entry| entry.is_array() && entry[0]["name"] == "branch")
        .unwrap();
    let nested = branch
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .find(|entry| entry.is_array() && entry[0]["name"] == "nested")
        .unwrap();
    assert!(
        nested
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .any(|entry| entry["name"] == "leaf" && entry["asize"] == 14)
    );
    assert!(
        tree.as_array()
            .unwrap()
            .iter()
            .skip(1)
            .any(|entry| entry.is_array()
                && entry[0]["name"] == "empty"
                && entry.as_array().unwrap().len() == 1)
    );
    assert!(records.iter().any(|record| record["name"] == unusual));
    for name in ["directory-link", "file-link"] {
        let link = tree
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .find(|entry| entry.is_object() && entry["name"] == name)
            .unwrap();
        assert_eq!(link["notreg"], true);
        assert_eq!(
            link["asize"],
            fs::symlink_metadata(root.join(name)).unwrap().len()
        );
    }
    let aliases: Vec<_> = records
        .iter()
        .filter(|record| record["name"] == "sparse" || record["name"] == "alias")
        .collect();
    assert_eq!(aliases.len(), 2);
    assert_eq!(
        aliases.iter().filter(|record| record["asize"] == 0).count(),
        1
    );
    for record in records {
        assert!(record.get("ino").is_none());
        assert!(record.get("dev").is_none());
        assert!(record.get("nlink").is_none());
    }
}

#[test]
fn cli_normalizes_current_and_child_paths_before_exporting() {
    let work = fixture();
    fs::create_dir_all(work.path().join("child/nested")).unwrap();
    fs::write(work.path().join("child/nested/payload"), b"contents").unwrap();
    let physical = work.path().canonicalize().unwrap();
    for (argument, suffix) in [
        (".", ""),
        ("./child", "child"),
        ("././child//./nested//.", "child/nested"),
        ("./child/nested/../.", "child/nested/.."),
    ] {
        let output = export_scan_from(work.path(), Path::new(argument));
        assert!(
            output.status.success(),
            "{argument}: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        let document = ncdu_document(&output);
        let expected = if suffix.is_empty() {
            physical.clone()
        } else {
            physical.join(suffix)
        };
        assert_eq!(
            document[3][0]["name"],
            expected.to_string_lossy().as_ref(),
            "{argument}",
        );
        let records = ncdu_records(&document[3]);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["name"] == "payload")
                .count(),
            1,
        );
        assert!(records.iter().all(|record| record["read_error"] != true));
    }
}

#[test]
fn cli_normalization_keeps_final_symlinks_as_leaves() {
    let work = fixture();
    fs::create_dir(work.path().join("target")).unwrap();
    fs::write(work.path().join("target/must-not-open"), b"outside").unwrap();
    symlink("target", work.path().join("alias")).unwrap();
    let physical = work.path().canonicalize().unwrap();
    for argument in ["./alias", "./alias/", "./alias/.", "././alias//./"] {
        let output = export_scan_from(work.path(), Path::new(argument));
        assert!(
            output.status.success(),
            "{argument}: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        let document = ncdu_document(&output);
        let records = ncdu_records(&document[3]);
        assert_eq!(records.len(), 2, "only the parent wrapper and symlink");
        assert_eq!(document[3][0]["name"], physical.to_string_lossy().as_ref());
        assert_eq!(document[3][1]["name"], "alias");
        assert_eq!(document[3][1]["notreg"], true);
        assert_eq!(document[3][1]["asize"], 6);
        assert!(records.iter().all(|record| record["read_error"] != true));
    }
    assert_eq!(
        fs::read(work.path().join("target/must-not-open")).unwrap(),
        b"outside"
    );
}

#[test]
fn headless_export_handles_non_utf8_paths_and_reports_missing_root() {
    let work = fixture();
    let raw_root = work.path().join(OsString::from_vec(b"root-\xfe".to_vec()));
    fs::create_dir(&raw_root).unwrap();
    let raw_dir = raw_root.join(OsString::from_vec(b"directory-\xff".to_vec()));
    fs::create_dir(&raw_dir).unwrap();
    let raw_file = raw_dir.join(OsString::from_vec(b"file-\xfd".to_vec()));
    fs::write(&raw_file, b"content").unwrap();
    let output = export_scan(&raw_root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = ncdu_document(&output);
    let records = ncdu_records(&document[3]);
    assert_eq!(records.len(), 3);
    assert_eq!(
        ncdu_totals(&records),
        reference(&raw_root, &mut HashSet::new()).0
    );
    assert_eq!(
        document[3][0]["name"],
        std::path::absolute(&raw_root)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert!(records.iter().any(|record| record["name"] == "directory-�"));
    assert!(
        records
            .iter()
            .any(|record| record["name"] == "file-�" && record["asize"] == 7)
    );
    assert!(records.iter().all(|record| record["read_error"] != true));

    let output = export_scan(&work.path().join("does-not-exist"));
    assert_eq!(output.status.code(), Some(2));
    let document = ncdu_document(&output);
    let records = ncdu_records(&document[3]);
    assert!(records.iter().any(|record| record["read_error"] == true));
    assert_eq!(ncdu_totals(&records), Sizes::default());
}

#[test]
fn export_file_matches_stdout_and_does_not_write_json_to_stdout() {
    let work = fixture();
    let root = work.path().join("tree");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("nested/leaf"), b"exported payload").unwrap();
    let stdout = export_scan(&root);
    assert!(
        stdout.status.success(),
        "{}",
        String::from_utf8_lossy(&stdout.stderr)
    );
    let mut expected = ncdu_document(&stdout);
    let path = work.path().join("scan.ncdu");
    let file = ldu_command()
        .args(["-0", "-o"])
        .arg(&path)
        .args(["--threads", "2"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        file.status.success(),
        "{}",
        String::from_utf8_lossy(&file.stderr)
    );
    assert!(file.stdout.is_empty());
    let mut actual: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(actual[2]["timestamp"].as_u64().is_some());
    actual[2].as_object_mut().unwrap().remove("timestamp");
    expected[2].as_object_mut().unwrap().remove("timestamp");
    assert_eq!(actual, expected);
}

#[test]
fn export_output_cannot_truncate_the_input_file_or_its_aliases() {
    let work = fixture();
    let input = work.path().join("input");
    let contents = b"original input must survive export rejection";
    fs::write(&input, contents).unwrap();
    let hardlink = work.path().join("hardlink");
    let symlink_path = work.path().join("symlink");
    fs::hard_link(&input, &hardlink).unwrap();
    symlink("input", &symlink_path).unwrap();
    for output in [&input, &hardlink, &symlink_path] {
        let result = ldu_command()
            .arg("-o")
            .arg(output)
            .arg(&input)
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "accepted input alias as output: {}",
            output.display()
        );
        assert_eq!(fs::read(&input).unwrap(), contents);
        assert_eq!(fs::read(&hardlink).unwrap(), contents);
        assert_eq!(fs::read(&symlink_path).unwrap(), contents);
        assert!(
            fs::symlink_metadata(&symlink_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn headless_export_wraps_a_file_root_without_charging_the_unscanned_parent() {
    let work = fixture();
    let root = work.path().join("only-file");
    fs::write(&root, b"standalone").unwrap();
    let output = export_scan(&root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = ncdu_document(&output);
    let records = ncdu_records(&document[3]);
    assert_eq!(records.len(), 2);
    assert_eq!(
        ncdu_totals(&records),
        reference(&root, &mut HashSet::new()).0
    );
    assert_eq!(document[3][1]["name"], "only-file");
    assert_eq!(document[3][1]["asize"], 10);
    assert_eq!(document[3][0]["asize"].as_u64().unwrap_or(0), 0);
    assert_eq!(document[3][0]["dsize"].as_u64().unwrap_or(0), 0);
}

fn json_prefix(input: &[u8]) -> (serde_json::Value, &[u8]) {
    let mut stream = serde_json::Deserializer::from_slice(input).into_iter();
    let value = stream.next().unwrap().unwrap();
    (value, &input[stream.byte_offset()..])
}

fn json_delimiter(input: &[u8], delimiter: u8) -> &[u8] {
    let input = input.trim_ascii_start();
    assert_eq!(input.first().copied(), Some(delimiter));
    &input[1..]
}

#[test]
fn headless_export_exports_deep_trees_without_recursive_serialization() {
    let work = fixture();
    let (_, expected) = deep_fixture(work.path(), 192, "", b"deep");
    let output = export_scan(work.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Parse one flat record at a time, independently of serde_json's nesting limit.
    let (major, input) = json_prefix(json_delimiter(&output.stdout, b'['));
    assert_eq!(major, 1);
    let (minor, input) = json_prefix(json_delimiter(input, b','));
    assert_eq!(minor, 2);
    let (metadata, input) = json_prefix(json_delimiter(input, b','));
    assert_eq!(metadata["progname"], "ldu");
    let mut input = json_delimiter(input, b',');
    let mut total = Sizes::default();
    for level in 0..=192 {
        let (record, remaining) = json_prefix(json_delimiter(input, b'['));
        assert!(record.is_object());
        if level != 0 {
            assert_eq!(record["name"], format!("d-{:03}", level - 1));
        }
        total.apparent += record["asize"].as_u64().unwrap_or(0);
        total.allocated += record["dsize"].as_u64().unwrap_or(0);
        input = json_delimiter(remaining, b',');
    }
    let (record, remaining) = json_prefix(input);
    assert_eq!(record["name"], "leaf");
    assert_eq!(record["asize"], 4);
    total.apparent += record["asize"].as_u64().unwrap_or(0);
    total.allocated += record["dsize"].as_u64().unwrap_or(0);
    input = remaining;
    // Every directory array plus the outer format array must close exactly once.
    for _ in 0..194 {
        input = json_delimiter(input, b']');
    }
    assert!(input.trim_ascii().is_empty());
    assert_eq!(total, expected);
}

#[cfg(target_os = "linux")]
fn delete_result(
    anchor: &std::sync::Arc<ldu::backend::RootAnchor>,
    relative: &Path,
) -> std::io::Result<()> {
    let handle = ldu::delete::spawn(std::sync::Arc::clone(anchor), relative.to_owned());
    let result = handle
        .receiver
        .recv_timeout(Duration::from_secs(15))
        .unwrap();
    assert_eq!(result.relative, relative);
    result.outcome
}

#[cfg(target_os = "linux")]
#[test]
fn confirmed_async_deletion_removes_owned_files_trees_and_links_without_following_targets() {
    let work = fixture();
    let root = work.path().join("root");
    let outside = work.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"preserve").unwrap();
    fs::write(root.join("file"), b"remove").unwrap();
    fs::create_dir_all(root.join("tree/nested")).unwrap();
    fs::write(root.join("tree/nested/leaf"), b"remove").unwrap();
    symlink("../../../outside", root.join("tree/nested/outside-link")).unwrap();
    symlink("../outside", root.join("outside-link")).unwrap();
    let raw_name = OsString::from_vec(b"delete-\xff".to_vec());
    fs::write(root.join(&raw_name), b"raw name").unwrap();
    let anchor = std::sync::Arc::new(ldu::backend::RootAnchor::open(&root).unwrap());

    for relative in [
        PathBuf::from("file"),
        PathBuf::from("tree"),
        PathBuf::from("outside-link"),
        PathBuf::from(raw_name),
    ] {
        delete_result(&anchor, &relative).unwrap();
        assert!(fs::symlink_metadata(root.join(relative)).is_err());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"preserve");
    }
    assert!(
        root.is_dir(),
        "confirmed child deletion must retain the scan root"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deletion_uses_the_original_root_anchor_and_rejects_symlinked_parent_replacement() {
    let work = fixture();
    let root = work.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("victim"), b"original").unwrap();
    let anchor = std::sync::Arc::new(ldu::backend::RootAnchor::open(&root).unwrap());
    let retained = work.path().join("retained-root");
    fs::rename(&root, &retained).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("victim"), b"replacement").unwrap();
    delete_result(&anchor, Path::new("victim")).unwrap();
    assert!(!retained.join("victim").exists());
    assert_eq!(fs::read(root.join("victim")).unwrap(), b"replacement");

    let outside = work.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"preserve").unwrap();
    symlink("../outside", retained.join("parent-link")).unwrap();
    let result = delete_result(&anchor, Path::new("parent-link/sentinel"));
    assert!(
        result.is_err(),
        "intermediate symlinks must not redirect deletion outside the anchored root"
    );
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"preserve");
}

#[cfg(target_os = "linux")]
#[test]
fn incremental_delete_keeps_the_scan_stream_and_promotes_a_late_hardlink_alias() {
    let work = fixture();
    let root = work.path();
    fs::write(root.join("owner"), b"hard link payload").unwrap();
    fs::create_dir_all(root.join("branch/deeper")).unwrap();
    fs::hard_link(root.join("owner"), root.join("branch/deeper/alias")).unwrap();
    // With one worker, the active branch must finish before its deeper task can
    // start. More single-entry batches than the bounded event channel can hold
    // keep that branch active while intake stops at the root owner, regardless
    // of enumeration order. The alias is checked only after deletion and intake.
    for n in 0..600 {
        fs::write(root.join("branch").join(format!("padding-{n:04}")), b"p").unwrap();
    }
    let handle = scan::spawn(
        root.to_owned(),
        ScanOptions {
            threads: 1,
            batch_size: 1,
            buffer_size: 4096,
            backend: Backend::Auto,
        },
    );
    let mut model = Model::new(root.to_owned());
    let (owner, owner_group) = loop {
        let event = handle
            .receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        let owner = match &event {
            ScanEvent::Entries {
                parent: ROOT,
                entries,
            } => entries
                .iter()
                .find(|entry| entry.name.as_ref() == "owner")
                .map(|entry| (entry.id, entry.hardlink_group.unwrap())),
            _ => None,
        };
        let _ = model.apply(event);
        if let Some(owner) = owner {
            break owner;
        }
    };
    assert!(!handle.is_finished() && !model.finished);
    assert!(!model.node(owner).hardlink_duplicate);
    let anchor = handle.anchor.get().unwrap().clone();
    let deletion = ldu::delete::spawn(anchor, PathBuf::from("owner"));
    let result = deletion
        .receiver
        .recv_timeout(Duration::from_secs(15))
        .unwrap();
    result.outcome.unwrap();
    assert!(result.metadata_error.is_none());
    assert_eq!(
        fs::symlink_metadata(root.join("branch/deeper/alias"))
            .unwrap()
            .nlink(),
        1
    );
    let _ = model.remove(owner);
    let parent = result.parent_metadata.unwrap();
    assert!(parent.relative.as_os_str().is_empty());
    let _ = model.update_directory_sizes(ROOT, parent.sizes);
    while model.has_pending_removals() {
        let _ = model.pump_removals();
    }
    let mut late_alias = None;
    while !model.finished {
        let event = handle
            .receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        if let ScanEvent::Entries { entries, .. } = &event {
            for entry in entries {
                if entry.name.as_ref() == "alias" {
                    assert_eq!(entry.hardlink_group, Some(owner_group));
                    late_alias = Some(entry.id);
                }
            }
        }
        let _ = model.apply(event);
        while model.has_pending_removals() {
            let _ = model.pump_removals();
        }
    }
    let alias = late_alias.unwrap();
    assert!(alias > owner && model.is_live(alias) && model.is_removed(owner));
    assert!(!model.node(alias).hardlink_duplicate);
    assert!(model.node(ROOT).complete && !model.cancelled);
    assert_eq!(model.node(ROOT).error_count, 0, "{:?}", model.last_error);
    let expected = reference(root, &mut HashSet::new());
    assert_eq!((model.node(ROOT).sizes, model.node(ROOT).items), expected);
    assert_eq!(
        (0..model.len() as NodeId)
            .filter(|&id| model.is_live(id))
            .count() as u64,
        expected.1 + 1
    );
    let branch = model
        .children(ROOT)
        .find(|&id| model.node(id).name.as_ref() == "branch")
        .unwrap();
    assert!(model.is_live(branch) && model.node(branch).complete);
}

#[test]
fn config_errors_preserve_exports_and_do_not_block_help_or_version() {
    let work = fixture();
    let root = work.path().join("tree");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("leaf"), b"fixture").unwrap();
    let xdg = work.path().join("settings");
    fs::create_dir_all(xdg.join("ldu")).unwrap();
    let xdg = xdg.canonicalize().unwrap();
    let config = xdg.join("ldu/config");
    fs::write(&config, "mouse_debounce_ms = invalid\n").unwrap();
    let export = work.path().join("existing.ncdu");
    fs::write(&export, b"preserve existing export").unwrap();
    let failed = ldu_command()
        .env("XDG_CONFIG_HOME", &xdg)
        .arg("-o")
        .arg(&export)
        .arg(&root)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(
        error.contains(&format!("{}:1:", config.display())),
        "{error}"
    );
    assert!(error.contains("mouse_debounce_ms"), "{error}");
    assert_eq!(fs::read(&export).unwrap(), b"preserve existing export");
    for flag in ["--help", "--version"] {
        let result = ldu_command()
            .env("XDG_CONFIG_HOME", &xdg)
            .arg(flag)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!result.stdout.is_empty());
    }
    let home = work.path().join("home");
    fs::create_dir_all(home.join(".config/ldu")).unwrap();
    let home = home.canonicalize().unwrap();
    fs::write(
        home.join(".config/ldu/config"),
        "backend=std\nthreads=1\nmouse=false\n",
    )
    .unwrap();
    let result = ldu_command()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "relative-is-ignored")
        .args(["--backend", "native", "-o", "-"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let document = ncdu_document(&result);
    assert!(
        ncdu_records(&document[3])
            .iter()
            .any(|record| record["name"] == "leaf")
    );
}
