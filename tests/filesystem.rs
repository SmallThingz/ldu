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

#[cfg(target_os = "linux")]
#[test]
fn scanner_traverses_deep_relative_paths_beyond_the_kernel_pathname_limit() {
    use std::os::fd::AsRawFd;

    let work = fixture();
    let root = work.path();
    let mut parent = File::open(root).unwrap();
    let mut relative = PathBuf::new();
    let mut expected = Sizes::default();
    let charge = |metadata: fs::Metadata| Sizes {
        apparent: metadata.len(),
        allocated: metadata.blocks() * 512,
    };
    // Keep every filesystem operation short while the logical path exceeds 4096 bytes.
    for n in 0..96 {
        let name = format!("layer-{n:03}-{}", "x".repeat(40));
        let short_parent = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let child = short_parent.join(&name);
        fs::create_dir(&child).unwrap();
        let own = charge(parent.metadata().unwrap());
        expected.apparent += own.apparent;
        expected.allocated += own.allocated;
        parent = File::open(child).unwrap();
        relative.push(name);
    }
    assert!(relative.as_os_str().as_encoded_bytes().len() > 4096);
    let short_leaf = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join("leaf");
    fs::write(&short_leaf, b"deep scanner payload").unwrap();
    for own in [
        charge(parent.metadata().unwrap()),
        charge(fs::symlink_metadata(&short_leaf).unwrap()),
    ] {
        expected.apparent += own.apparent;
        expected.allocated += own.allocated;
    }
    let leaf_path = root.join(relative).join("leaf");
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

fn json_scan(path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ldu"))
        .args(["--json", "--threads", "2"])
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn headless_json_handles_non_utf8_root_and_directory_paths_and_reports_missing_root() {
    let work = fixture();
    let raw_root = work.path().join(OsString::from_vec(b"root-\xfe".to_vec()));
    fs::create_dir(&raw_root).unwrap();
    let raw_dir = raw_root.join(OsString::from_vec(b"directory-\xff".to_vec()));
    fs::create_dir(&raw_dir).unwrap();
    fs::write(raw_dir.join("leaf"), b"content").unwrap();
    let output = json_scan(&raw_root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["errors"], 0);
    assert_eq!(json["entries"], 2);
    assert_eq!(json["directories"][0]["complete"], true);
    let expected = reference(&raw_root, &mut HashSet::new()).0;
    assert_eq!(json["apparent"], expected.apparent);
    assert_eq!(json["allocated"], expected.allocated);

    let output = json_scan(&work.path().join("does-not-exist"));
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["errors"].as_u64().unwrap() > 0);
    assert_eq!(json["cancelled"], false);
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
