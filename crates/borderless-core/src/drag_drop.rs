use crate::geometry::Point;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropResolutionKind {
    DesktopHit,
    ExplorerHit,
    ExplorerFallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropTarget {
    pub destination_dir: String,
    pub resolution_kind: DropResolutionKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragDropPhase {
    LocalDragDetected,
    RemoteTargetSelecting,
    RemoteDropReleased { release_point: Point },
    RemoteTargetResolved { target: DropTarget },
    Transferring { target: DropTarget },
    Completed,
    Cancelled,
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragDropSession {
    session_id: Uuid,
    transfer_id: Uuid,
    source_paths: Vec<String>,
    phase: DragDropPhase,
}

impl DragDropSession {
    pub fn new(session_id: Uuid, transfer_id: Uuid, source_paths: Vec<String>) -> Self {
        Self {
            session_id,
            transfer_id,
            source_paths,
            phase: DragDropPhase::LocalDragDetected,
        }
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    pub fn transfer_id(&self) -> Uuid {
        self.transfer_id
    }

    pub fn source_paths(&self) -> &[String] {
        &self.source_paths
    }

    pub fn phase(&self) -> &DragDropPhase {
        &self.phase
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            DragDropPhase::Completed | DragDropPhase::Cancelled | DragDropPhase::Failed { .. }
        )
    }

    pub fn begin_remote_target_selection(&mut self) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::LocalDragDetected),
            DragDropPhase::RemoteTargetSelecting,
        )
    }

    pub fn release_at(&mut self, release_point: Point) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::RemoteTargetSelecting),
            DragDropPhase::RemoteDropReleased { release_point },
        )
    }

    pub fn resolve_target(&mut self, target: DropTarget) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::RemoteDropReleased { .. }),
            DragDropPhase::RemoteTargetResolved { target },
        )
    }

    pub fn begin_transfer(&mut self) -> Result<(), DragDropStateError> {
        let DragDropPhase::RemoteTargetResolved { target } = &self.phase else {
            return Err(self.invalid("begin_transfer"));
        };
        self.phase = DragDropPhase::Transferring {
            target: target.clone(),
        };
        Ok(())
    }

    pub fn complete(&mut self) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::Transferring { .. }),
            DragDropPhase::Completed,
        )
    }

    pub fn cancel(&mut self) -> Result<(), DragDropStateError> {
        if self.is_terminal() {
            return Err(self.invalid("cancel"));
        }
        self.phase = DragDropPhase::Cancelled;
        Ok(())
    }

    pub fn fail(&mut self, reason: impl Into<String>) -> Result<(), DragDropStateError> {
        if self.is_terminal() {
            return Err(self.invalid("fail"));
        }
        self.phase = DragDropPhase::Failed {
            reason: reason.into(),
        };
        Ok(())
    }

    fn transition(&mut self, allowed: bool, next: DragDropPhase) -> Result<(), DragDropStateError> {
        if !allowed {
            return Err(DragDropStateError {
                session_id: self.session_id,
                from: self.phase.clone(),
                attempted: format!("{next:?}"),
            });
        }
        self.phase = next;
        Ok(())
    }

    fn invalid(&self, attempted: &str) -> DragDropStateError {
        DragDropStateError {
            session_id: self.session_id,
            from: self.phase.clone(),
            attempted: attempted.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragDropStateError {
    pub session_id: Uuid,
    pub from: DragDropPhase,
    pub attempted: String,
}

impl std::fmt::Display for DragDropStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid drag-drop transition for session {}: {} from {:?}",
            self.session_id, self.attempted, self.from
        )
    }
}

impl std::error::Error for DragDropStateError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> DragDropSession {
        DragDropSession::new(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\a.txt".to_string()],
        )
    }

    fn target() -> DropTarget {
        DropTarget {
            destination_dir: "C:\\Users\\demo\\Desktop".to_string(),
            resolution_kind: DropResolutionKind::DesktopHit,
        }
    }

    #[test]
    fn happy_path_walks_all_phases() {
        let mut s = session();
        assert_eq!(*s.phase(), DragDropPhase::LocalDragDetected);

        s.begin_remote_target_selection().unwrap();
        s.release_at(Point::new(100, 200)).unwrap();
        assert_eq!(
            *s.phase(),
            DragDropPhase::RemoteDropReleased {
                release_point: Point::new(100, 200)
            }
        );

        s.resolve_target(target()).unwrap();
        s.begin_transfer().unwrap();
        s.complete().unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn release_before_remote_selection_is_rejected() {
        let mut s = session();
        assert!(s.release_at(Point::new(0, 0)).is_err());
        assert_eq!(*s.phase(), DragDropPhase::LocalDragDetected);
    }

    #[test]
    fn transfer_requires_resolved_target() {
        let mut s = session();
        s.begin_remote_target_selection().unwrap();
        assert!(s.begin_transfer().is_err());
    }

    #[test]
    fn cancel_allowed_in_any_non_terminal_phase() {
        let mut s = session();
        s.begin_remote_target_selection().unwrap();
        s.cancel().unwrap();
        assert_eq!(*s.phase(), DragDropPhase::Cancelled);
        assert!(s.cancel().is_err());
    }

    #[test]
    fn fail_records_reason_and_is_terminal() {
        let mut s = session();
        s.begin_remote_target_selection().unwrap();
        s.release_at(Point::new(5, 5)).unwrap();
        s.fail("no destination resolved").unwrap();
        assert_eq!(
            *s.phase(),
            DragDropPhase::Failed {
                reason: "no destination resolved".to_string()
            }
        );
        assert!(s.is_terminal());
        assert!(s.fail("again").is_err());
    }
}
