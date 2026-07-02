use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DragDropState {
    LocalDragDetected,
    TransferringFiles,
    RemoteDragReady,
    RemoteDragging,
    Dropped,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DragDropSession {
    pub session_id: Uuid,
    pub transfer_id: Uuid,
    pub state: DragDropState,
}
