use std::{ffi::OsStr, sync::Arc};

pub type NodeId = u32;
/// Filesystem identity carried by bounded scan events; the model owns deduplication.
pub type FileIdentity = (u64, u64);
pub const ROOT: NodeId = 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub apparent: u64,
    pub allocated: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
    Other,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SizeMode {
    #[default]
    Allocated,
    Apparent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMode {
    #[default]
    Size,
    Name,
    Items,
}

#[derive(Debug)]
pub struct Entry {
    pub id: NodeId,
    pub name: Arc<OsStr>,
    pub kind: NodeKind,
    pub sizes: Sizes,
    pub hardlink_group: Option<FileIdentity>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum ScanEvent {
    RootInfo {
        /// Device/inode identity for a regular-file scan root.
        file_identity: Option<FileIdentity>,
        kind: NodeKind,
        sizes: Sizes,
        error: Option<String>,
    },
    Entries {
        parent: NodeId,
        entries: Vec<Entry>,
    },
    DirectoryDone {
        id: NodeId,
        error: Option<String>,
    },
    Finished {
        cancelled: bool,
    },
}
