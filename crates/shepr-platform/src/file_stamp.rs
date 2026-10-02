use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

/// Linux file identity and timestamps used to detect replacement or mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileStamp {
    /// Captures device, inode, size, and nanosecond modification and change times.
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}
