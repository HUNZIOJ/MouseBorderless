use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifestEntry {
    pub relative_path: String,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub blake3_hex: Option<String>,
}

impl FileManifestEntry {
    pub fn file(relative_path: impl Into<String>, size_bytes: u64) -> Self {
        Self {
            relative_path: relative_path.into(),
            size_bytes,
            is_dir: false,
            blake3_hex: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferManifest {
    pub transfer_id: Uuid,
    pub root_name: String,
    pub files: Vec<FileManifestEntry>,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    pub transfer_id: Uuid,
    pub relative_path: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub blake3_hex: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileTransferState {
    Offered,
    Transferring,
    Completed,
    Cancelled,
    Failed,
}
