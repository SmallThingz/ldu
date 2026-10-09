use serde::Serialize;
use std::ffi::OsString;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Sizes {
    pub apparent: u64,
    pub allocated: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
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
    pub name: OsString,
    pub kind: NodeKind,
    pub sizes: Sizes,
    pub hardlink_duplicate: bool,
    pub hardlink_group: Option<NodeId>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum ScanEvent {
    RootInfo {
        /// Opaque canonical hardlink group for a regular-file scan root.
        file_identity: Option<NodeId>,
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
