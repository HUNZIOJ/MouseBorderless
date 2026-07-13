use crate::geometry::Point;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragDirection {
    ControllerToAgent,
    AgentToController,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropResolutionKind {
    Desktop,
    ExplorerDirectory,
    FolderIcon,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropTargetSummary {
    pub display_name: String,
    pub resolution_kind: DropResolutionKind,
    pub authorization: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragDropPhase {
    LocalDragDetected,
    RemoteTargetSelecting,
    RemoteDropReleased { release_point: Point },
    RemoteTargetResolved { target: DropTargetSummary },
    Transferring,
    Completed,
    Cancelled { reason: String },
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragDropSession {
    session_id: Uuid,
    transfer_id: Uuid,
    direction: DragDirection,
    source_paths: Vec<String>,
    phase: DragDropPhase,
}

impl DragDropSession {
    pub fn new(
        session_id: Uuid,
        transfer_id: Uuid,
        direction: DragDirection,
        source_paths: Vec<String>,
    ) -> Self {
        Self {
            session_id,
            transfer_id,
            direction,
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

    pub fn direction(&self) -> DragDirection {
        self.direction
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
            DragDropPhase::Completed
                | DragDropPhase::Cancelled { .. }
                | DragDropPhase::Failed { .. }
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

    pub fn resolve_target(&mut self, target: DropTargetSummary) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::RemoteDropReleased { .. }),
            DragDropPhase::RemoteTargetResolved { target },
        )
    }

    pub fn begin_transfer(&mut self) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::RemoteTargetResolved { .. }),
            DragDropPhase::Transferring,
        )
    }

    pub fn complete(&mut self) -> Result<(), DragDropStateError> {
        self.transition(
            matches!(self.phase, DragDropPhase::Transferring),
            DragDropPhase::Completed,
        )
    }

    pub fn cancel(&mut self, reason: impl Into<String>) -> Result<(), DragDropStateError> {
        if self.is_terminal() {
            return Err(self.invalid("cancel"));
        }
        self.phase = DragDropPhase::Cancelled {
            reason: reason.into(),
        };
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

    fn test_session(direction: DragDirection) -> DragDropSession {
        DragDropSession::new(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            direction,
            vec!["C:\\src\\a.txt".to_string()],
        )
    }

    fn test_target() -> DropTargetSummary {
        DropTargetSummary {
            display_name: "Desktop".to_string(),
            resolution_kind: DropResolutionKind::Desktop,
            authorization: Uuid::from_u128(3),
        }
    }

    #[test]
    fn both_directions_follow_the_same_happy_path() {
        for direction in [
            DragDirection::ControllerToAgent,
            DragDirection::AgentToController,
        ] {
            let mut session = DragDropSession::new(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                direction,
                vec!["C:\\src\\report.pdf".to_string()],
            );
            session.begin_remote_target_selection().unwrap();
            session.release_at(Point::new(200, 300)).unwrap();
            session.resolve_target(test_target()).unwrap();
            session.begin_transfer().unwrap();
            session.complete().unwrap();
            assert_eq!(session.phase(), &DragDropPhase::Completed);
        }
    }

    #[test]
    fn sent_bytes_do_not_complete_the_drag_session() {
        let mut session = test_session(DragDirection::ControllerToAgent);
        session.begin_remote_target_selection().unwrap();
        session.release_at(Point::new(10, 20)).unwrap();
        session.resolve_target(test_target()).unwrap();
        session.begin_transfer().unwrap();

        assert_eq!(session.phase(), &DragDropPhase::Transferring);
        assert!(!session.is_terminal());
    }

    #[test]
    fn release_before_remote_selection_is_rejected() {
        let mut s = test_session(DragDirection::ControllerToAgent);
        assert!(s.release_at(Point::new(0, 0)).is_err());
        assert_eq!(*s.phase(), DragDropPhase::LocalDragDetected);
    }

    #[test]
    fn transfer_requires_resolved_target() {
        let mut s = test_session(DragDirection::ControllerToAgent);
        s.begin_remote_target_selection().unwrap();
        assert!(s.begin_transfer().is_err());
    }

    #[test]
    fn cancel_allowed_in_any_non_terminal_phase() {
        let mut s = test_session(DragDirection::ControllerToAgent);
        s.begin_remote_target_selection().unwrap();
        s.cancel("user cancelled").unwrap();
        assert_eq!(
            *s.phase(),
            DragDropPhase::Cancelled {
                reason: "user cancelled".to_string()
            }
        );
        assert!(s.cancel("again").is_err());
    }

    #[test]
    fn fail_records_reason_and_is_terminal() {
        let mut s = test_session(DragDirection::AgentToController);
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
