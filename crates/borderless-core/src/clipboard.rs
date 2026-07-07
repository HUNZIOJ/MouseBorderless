use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

use crate::file_transfer::FileManifestEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipboardChangeId {
    pub source_device_id: Uuid,
    pub sequence: u64,
}

impl ClipboardChangeId {
    pub fn new(source_device_id: Uuid, sequence: u64) -> Self {
        Self {
            source_device_id,
            sequence,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClipboardLoopGuard {
    seen: BTreeSet<ClipboardChangeId>,
}

impl ClipboardLoopGuard {
    pub fn accept(&mut self, change_id: ClipboardChangeId) -> bool {
        self.seen.insert(change_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardPayload {
    UnicodeText(String),
    Html(String),
    ImagePng(Vec<u8>),
    ImageDib(Vec<u8>),
    Files(RemoteFileOffer),
}

impl ClipboardPayload {
    pub fn total_bytes(&self) -> u64 {
        match self {
            Self::UnicodeText(value) | Self::Html(value) => value.len() as u64,
            Self::ImagePng(bytes) | Self::ImageDib(bytes) => bytes.len() as u64,
            Self::Files(offer) => offer
                .files
                .iter()
                .fold(0u64, |total, file| total.saturating_add(file.size_bytes)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFileOffer {
    pub transfer_id: Uuid,
    pub files: Vec<FileManifestEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEnvelope {
    pub change_id: ClipboardChangeId,
    pub payload: ClipboardPayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_clipboard_change_from_same_source_is_ignored() {
        let source = uuid::Uuid::new_v4();
        let change = ClipboardChangeId::new(source, 7);
        let mut guard = ClipboardLoopGuard::default();
        assert!(guard.accept(change));
        assert!(!guard.accept(change));
    }

    #[test]
    fn file_clipboard_offer_reports_total_size() {
        let offer = ClipboardPayload::Files(RemoteFileOffer {
            transfer_id: uuid::Uuid::new_v4(),
            files: vec![
                FileManifestEntry::file("a.txt", 10),
                FileManifestEntry::file("dir/b.txt", 20),
            ],
        });
        assert_eq!(offer.total_bytes(), 30);
    }

    #[test]
    fn file_clipboard_offer_total_size_saturates_on_overflow() {
        let offer = ClipboardPayload::Files(RemoteFileOffer {
            transfer_id: uuid::Uuid::new_v4(),
            files: vec![
                FileManifestEntry::file("huge.bin", u64::MAX),
                FileManifestEntry::file("another-byte.bin", 1),
            ],
        });
        assert_eq!(offer.total_bytes(), u64::MAX);
    }
}
