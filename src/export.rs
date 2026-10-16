//! Ncdu 1.2 JSON tree export. Names use lossy UTF-8, as the former JSON summary did.

use crate::{
    model::Model,
    types::{NodeId, NodeKind, ROOT, Sizes},
};
use serde::Serialize;
use std::{
    borrow::Cow,
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
struct Metadata {
    progname: &'static str,
    progver: &'static str,
    timestamp: u64,
}

#[derive(Serialize)]
struct Record<'a> {
    name: Cow<'a, str>,
    asize: u64,
    dsize: u64,
    #[serde(skip_serializing_if = "is_false")]
    notreg: bool,
    #[serde(skip_serializing_if = "is_false")]
    read_error: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl<'a> Record<'a> {
    fn new(name: Cow<'a, str>, sizes: Sizes) -> Self {
        Self {
            name,
            asize: sizes.apparent,
            dsize: sizes.allocated,
            notreg: false,
            read_error: false,
        }
    }

    fn node(model: &'a Model, id: NodeId, root: &'a Path) -> Self {
        let node = model.node(id);
        let name = if id == ROOT && node.kind == NodeKind::Directory {
            root.as_os_str()
        } else {
            node.name.as_ref()
        };
        let mut record = Self::new(name.to_string_lossy(), model.own_sizes(id));
        record.notreg = matches!(node.kind, NodeKind::Symlink | NodeKind::Other);
        record.read_error = node.error_count != 0
            || !node.complete
            || model.has_partial_totals(id)
            || (id == ROOT && (!model.finished || model.cancelled));
        record
    }
}

/// Write the complete tree with constant traversal space and buffered output.
///
/// Directory records contain their own metadata sizes, not subtree totals.
/// Hardlink aliases export their counted zero bytes; this format intentionally
/// omits inode and device metadata.
/// Ncdu requires a directory root, so a file root is wrapped in its parent.
pub fn write_ncdu(model: &Model, root: &Path, output: impl Write) -> io::Result<()> {
    let mut output = io::BufWriter::new(output);
    output.write_all(b"[1,2,")?;
    serde_json::to_writer(
        &mut output,
        &Metadata {
            progname: "ldu",
            progver: env!("CARGO_PKG_VERSION"),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    )?;
    output.write_all(b",")?;
    let wrapped = model.node(ROOT).kind != NodeKind::Directory;
    if wrapped {
        output.write_all(b"[")?;
        serde_json::to_writer(
            &mut output,
            &Record::new(
                root.parent().unwrap_or(root).to_string_lossy(),
                Sizes::default(),
            ),
        )?;
        output.write_all(b",")?;
    }

    // The model already stores the traversal stack in its parent/sibling links.
    // Never build a recursive serialization tree or materialize per-directory lists.
    let mut id = ROOT;
    'entries: loop {
        let directory = model.node(id).kind == NodeKind::Directory;
        if directory {
            output.write_all(b"[")?;
        }
        serde_json::to_writer(&mut output, &Record::node(model, id, root))?;
        if directory && let Some(child) = model.first_child(id) {
            output.write_all(b",")?;
            id = child;
            continue;
        }
        loop {
            if model.node(id).kind == NodeKind::Directory {
                output.write_all(b"]")?;
            }
            if id == ROOT {
                break 'entries;
            }
            if let Some(sibling) = model.next_sibling(id) {
                output.write_all(b",")?;
                id = sibling;
                continue 'entries;
            }
            id = model.node(id).parent.expect("non-root entry has a parent");
        }
    }
    if wrapped {
        output.write_all(b"]")?;
    }
    output.write_all(b"]\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Entry, ScanEvent};
    use std::{ffi::OsStr, path::PathBuf};

    fn root_info(model: &mut Model, kind: NodeKind, sizes: Sizes) {
        model.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind,
            sizes,
            error: None,
        });
    }

    fn entry(id: NodeId, name: &str, kind: NodeKind, sizes: Sizes) -> Entry {
        Entry {
            id,
            name: OsStr::new(name).into(),
            kind,
            sizes,
            hardlink_group: None,
            error: None,
        }
    }

    fn export(model: &Model, root: &Path) -> serde_json::Value {
        let mut output = Vec::new();
        write_ncdu(model, root, &mut output).unwrap();
        serde_json::from_slice(&output).unwrap()
    }

    #[test]
    fn directories_export_retained_own_sizes_even_when_totals_saturate() {
        let root = Path::new("/fixture");
        let mut model = Model::new(root.into());
        let own = Sizes {
            apparent: 17,
            allocated: 512,
        };
        root_info(&mut model, NodeKind::Directory, own);
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(
                1,
                "large",
                NodeKind::File,
                Sizes {
                    apparent: u64::MAX,
                    allocated: u64::MAX,
                },
            )],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        model.apply(ScanEvent::Finished { cancelled: false });
        let value = export(&model, root);
        assert_eq!(value[0], 1);
        assert_eq!(value[1], 2);
        assert_eq!(value[2]["progname"], "ldu");
        assert_eq!(value[3][0]["asize"], own.apparent);
        assert_eq!(value[3][0]["dsize"], own.allocated);
        assert_eq!(value[3][0]["read_error"], true);
        assert_eq!(value[3][1]["asize"], u64::MAX);
    }

    #[test]
    fn cancelled_and_unfinished_exports_keep_partial_marks() {
        let root = Path::new("/fixture");
        let mut model = Model::new(root.into());
        root_info(&mut model, NodeKind::Directory, Sizes::default());
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "partial", NodeKind::Directory, Sizes::default())],
        });
        for cancelled in [false, true] {
            if cancelled {
                model.apply(ScanEvent::Finished { cancelled: true });
            }
            let value = export(&model, root);
            assert_eq!(value[3][0]["read_error"], true);
            assert_eq!(value[3][1][0]["read_error"], true);
        }
    }

    #[test]
    fn deep_export_does_not_use_a_recursive_call_stack() {
        const DEPTH: NodeId = 1024;
        let root = PathBuf::from("/fixture");
        let mut model = Model::new(root.clone());
        root_info(&mut model, NodeKind::Directory, Sizes::default());
        for id in 1..=DEPTH {
            model.apply(ScanEvent::Entries {
                parent: id - 1,
                entries: vec![entry(id, "d", NodeKind::Directory, Sizes::default())],
            });
        }
        for id in (ROOT..=DEPTH).rev() {
            model.apply(ScanEvent::DirectoryDone { id, error: None });
        }
        model.apply(ScanEvent::Finished { cancelled: false });
        let output = std::thread::Builder::new()
            .stack_size(96 * 1024)
            .spawn(move || {
                let mut output = Vec::new();
                write_ncdu(&model, &root, &mut output).unwrap();
                output
            })
            .unwrap()
            .join()
            .unwrap();

        // Parse each record independently so the verifier also has bounded depth.
        let mut tail = output.strip_prefix(b"[1,2,").unwrap();
        let mut metadata =
            serde_json::Deserializer::from_slice(tail).into_iter::<serde_json::Value>();
        assert_eq!(metadata.next().unwrap().unwrap()["progname"], "ldu");
        tail = &tail[metadata.byte_offset()..];
        for id in ROOT..=DEPTH {
            tail = tail.strip_prefix(b",[").unwrap();
            let mut record =
                serde_json::Deserializer::from_slice(tail).into_iter::<serde_json::Value>();
            let value = record.next().unwrap().unwrap();
            assert_eq!(value["name"], if id == ROOT { "/fixture" } else { "d" });
            assert!(value.get("read_error").is_none());
            tail = &tail[record.byte_offset()..];
        }
        assert_eq!(tail.len(), DEPTH as usize + 3);
        assert!(tail[..tail.len() - 1].iter().all(|&byte| byte == b']'));
        assert_eq!(tail.last(), Some(&b'\n'));
    }

    #[test]
    fn buffered_output_errors_are_returned() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("output unavailable"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = Path::new("/fixture");
        assert!(write_ncdu(&Model::new(root.into()), root, Fail).is_err());
    }
}
