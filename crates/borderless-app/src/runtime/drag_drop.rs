use borderless_core::{
    config::{RemotePosition, Role},
    control::ControlMode,
    drag_drop::{DragDirection, DragDropPhase, DragDropSession},
    geometry::{edge_for_position, Edge, Point},
    protocol::WireMessage,
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DragEffect {
    InstallEdge(Edge),
    UninstallEdge,
    Send(WireMessage),
    AwaitLocalRelease {
        session_id: Uuid,
    },
    ResolveTarget {
        session_id: Uuid,
        point: Point,
    },
    BeginTransfer {
        session_id: Uuid,
        transfer_id: Uuid,
        authorization: Uuid,
    },
    CancelBulk {
        transfer_id: Uuid,
    },
    RevokeAuthorization {
        session_id: Uuid,
    },
    RestoreLocalInput,
    Status {
        state: String,
        destination: Option<String>,
    },
}

struct PeerDragOffer {
    session_id: Uuid,
    _transfer_id: Uuid,
    _item_count: u32,
    handed_off: bool,
}

pub struct DragDropCoordinator {
    role: Role,
    control_mode: ControlMode,
    peer_layout: Option<RemotePosition>,
    active: Option<DragDropSession>,
    peer_offer: Option<PeerDragOffer>,
    #[allow(dead_code)]
    connection_generation: u64,
}

impl DragDropCoordinator {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            control_mode: ControlMode::Local,
            peer_layout: None,
            active: None,
            peer_offer: None,
            connection_generation: 0,
        }
    }

    pub fn set_control_mode(&mut self, mode: ControlMode) -> Vec<DragEffect> {
        self.control_mode = mode;
        let mut effects = Vec::new();

        if self.role == Role::Controller {
            let phase = self.active.as_ref().map(DragDropSession::phase);
            match (mode, phase) {
                (ControlMode::Remote, Some(DragDropPhase::LocalDragDetected)) => {
                    if let Some(session) = self.active.as_mut() {
                        let _ = session.begin_remote_target_selection();
                    }
                    effects.push(DragEffect::Status {
                        state: "正在选择目标目录".to_string(),
                        destination: None,
                    });
                }
                (ControlMode::Local, Some(DragDropPhase::RemoteTargetSelecting)) => {
                    if let Some(mut session) = self.active.take() {
                        let session_id = session.session_id();
                        let reason = "拖拽已返回本机".to_string();
                        let _ = session.cancel(reason.clone());
                        effects.push(DragEffect::Send(WireMessage::DragDropCancel {
                            session_id,
                            reason: reason.clone(),
                        }));
                        effects.push(DragEffect::Status {
                            state: format!("拖放已取消：{reason}"),
                            destination: None,
                        });
                    }
                }
                _ => {}
            }
        }

        effects.extend(self.local_release_handoff_effects());
        effects
    }

    #[cfg(test)]
    pub fn active_session(&self) -> Option<&DragDropSession> {
        self.active.as_ref()
    }

    pub fn begin_local_drag(
        &mut self,
        session_id: Uuid,
        transfer_id: Uuid,
        source_paths: Vec<String>,
    ) -> Vec<DragEffect> {
        if self.active.is_some() || self.peer_offer.is_some() {
            return vec![DragEffect::Send(WireMessage::DragDropCancel {
                session_id,
                reason: "another drag-drop session is active".to_string(),
            })];
        }

        let item_count = u32::try_from(source_paths.len()).unwrap_or(u32::MAX);
        let direction = match self.role {
            Role::Controller => DragDirection::ControllerToAgent,
            Role::Agent => DragDirection::AgentToController,
        };
        let mut session = DragDropSession::new(session_id, transfer_id, direction, source_paths);
        if self.role == Role::Agent {
            let _ = session.begin_remote_target_selection();
        }
        self.active = Some(session);

        let mut effects = vec![
            DragEffect::Send(WireMessage::DragDropEntered {
                session_id,
                transfer_id,
                item_count,
            }),
            DragEffect::Status {
                state: "文件已到达共享边缘".to_string(),
                destination: None,
            },
        ];
        if self.role == Role::Agent {
            effects.push(DragEffect::Status {
                state: "正在选择目标目录".to_string(),
                destination: None,
            });
        }
        effects
    }

    pub fn cancel_local_drag(
        &mut self,
        session_id: Uuid,
        reason: impl Into<String>,
    ) -> Vec<DragEffect> {
        let Some(mut session) = self
            .active
            .take_if(|session| session.session_id() == session_id)
        else {
            return Vec::new();
        };
        let reason = reason.into();
        let _ = session.cancel(reason.clone());
        vec![
            DragEffect::Send(WireMessage::DragDropCancel {
                session_id,
                reason: reason.clone(),
            }),
            DragEffect::Status {
                state: format!("拖放已取消：{reason}"),
                destination: None,
            },
        ]
    }

    pub fn native_drop_released(&mut self, session_id: Uuid) -> Vec<DragEffect> {
        let Some(session) = self
            .active
            .as_ref()
            .filter(|session| session.session_id() == session_id)
        else {
            return Vec::new();
        };

        if session.phase() == &DragDropPhase::LocalDragDetected {
            return self.cancel_local_drag(session_id, "尚未进入另一台电脑就已松开");
        }

        vec![DragEffect::Status {
            state: "拖拽已交接到另一台电脑".to_string(),
            destination: None,
        }]
    }

    pub fn reset(&mut self) {
        self.active = None;
        self.peer_offer = None;
    }

    pub fn handle_peer_message(&mut self, message: WireMessage) -> Vec<DragEffect> {
        match message {
            WireMessage::PeerLayout {
                controller_remote_position,
            } => {
                let edge = edge_for_position(controller_remote_position.clone());
                self.peer_layout = Some(controller_remote_position);
                vec![DragEffect::InstallEdge(if self.role == Role::Controller {
                    edge
                } else {
                    edge.opposite()
                })]
            }
            WireMessage::DragDropEntered {
                session_id,
                transfer_id,
                item_count,
            } => {
                if self.active.is_some() || self.peer_offer.is_some() {
                    return vec![DragEffect::Send(WireMessage::DragDropCancel {
                        session_id,
                        reason: "another drag-drop session is active".to_string(),
                    })];
                }
                self.peer_offer = Some(PeerDragOffer {
                    session_id,
                    _transfer_id: transfer_id,
                    _item_count: item_count,
                    handed_off: false,
                });
                self.local_release_handoff_effects()
            }
            _ => Vec::new(),
        }
    }

    fn local_release_handoff_effects(&mut self) -> Vec<DragEffect> {
        if self.role != Role::Controller || self.control_mode != ControlMode::Local {
            return Vec::new();
        }
        let Some(offer) = self.peer_offer.as_mut() else {
            return Vec::new();
        };
        if offer.handed_off {
            return Vec::new();
        }
        offer.handed_off = true;
        vec![DragEffect::AwaitLocalRelease {
            session_id: offer.session_id,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::{
        config::{RemotePosition, Role},
        control::ControlMode,
        drag_drop::{DragDirection, DragDropPhase},
        geometry::Edge,
        protocol::WireMessage,
    };
    use uuid::Uuid;

    #[test]
    fn agent_entered_after_control_returns_is_immediately_handed_to_controller() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.set_control_mode(ControlMode::Local);

        let effects = coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id: Uuid::from_u128(1),
            transfer_id: Uuid::from_u128(2),
            item_count: 1,
        });

        assert!(effects.contains(&DragEffect::AwaitLocalRelease {
            session_id: Uuid::from_u128(1),
        }));
    }

    #[test]
    fn control_returns_after_agent_entered_produces_the_same_effect() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.set_control_mode(ControlMode::Remote);
        coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id: Uuid::from_u128(1),
            transfer_id: Uuid::from_u128(2),
            item_count: 1,
        });

        let effects = coordinator.set_control_mode(ControlMode::Local);
        assert!(effects.contains(&DragEffect::AwaitLocalRelease {
            session_id: Uuid::from_u128(1),
        }));
    }

    #[test]
    fn layout_installs_only_the_reciprocal_agent_edge() {
        let effects =
            DragDropCoordinator::new(Role::Agent).handle_peer_message(WireMessage::PeerLayout {
                controller_remote_position: RemotePosition::Right,
            });
        assert!(effects.contains(&DragEffect::InstallEdge(Edge::Left)));
    }

    #[test]
    fn both_roles_create_the_same_source_session_shape() {
        for (role, direction) in [
            (Role::Controller, DragDirection::ControllerToAgent),
            (Role::Agent, DragDirection::AgentToController),
        ] {
            let mut coordinator = DragDropCoordinator::new(role);
            let effects = coordinator.begin_local_drag(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                vec!["C:\\src\\report.pdf".to_string()],
            );

            assert!(
                effects.contains(&DragEffect::Send(WireMessage::DragDropEntered {
                    session_id: Uuid::from_u128(1),
                    transfer_id: Uuid::from_u128(2),
                    item_count: 1,
                }))
            );
            assert_eq!(coordinator.active_session().unwrap().direction(), direction);
        }
    }

    #[test]
    fn second_peer_offer_is_rejected_while_one_is_active() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id: Uuid::from_u128(1),
            transfer_id: Uuid::from_u128(2),
            item_count: 1,
        });

        let effects = coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id: Uuid::from_u128(3),
            transfer_id: Uuid::from_u128(4),
            item_count: 1,
        });

        assert_eq!(
            effects,
            vec![DragEffect::Send(WireMessage::DragDropCancel {
                session_id: Uuid::from_u128(3),
                reason: "another drag-drop session is active".to_string(),
            })]
        );
    }

    #[test]
    fn controller_source_starts_remote_selection_when_control_crosses_the_edge() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.begin_local_drag(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

        let effects = coordinator.set_control_mode(ControlMode::Remote);

        assert_eq!(
            coordinator.active_session().unwrap().phase(),
            &DragDropPhase::RemoteTargetSelecting
        );
        assert!(effects.contains(&DragEffect::Status {
            state: "正在选择目标目录".to_string(),
            destination: None,
        }));
    }

    #[test]
    fn local_cancel_notifies_the_peer_and_clears_the_session() {
        let mut coordinator = DragDropCoordinator::new(Role::Agent);
        coordinator.begin_local_drag(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

        let effects = coordinator.cancel_local_drag(Uuid::from_u128(1), "用户取消");

        assert!(coordinator.active_session().is_none());
        assert!(
            effects.contains(&DragEffect::Send(WireMessage::DragDropCancel {
                session_id: Uuid::from_u128(1),
                reason: "用户取消".to_string(),
            }))
        );
    }

    #[test]
    fn release_on_the_edge_before_handoff_cancels_the_source() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.begin_local_drag(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

        let effects = coordinator.native_drop_released(Uuid::from_u128(1));

        assert!(coordinator.active_session().is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DragEffect::Status { state, .. } if state.contains("尚未进入另一台电脑")
        )));
    }

    #[test]
    fn handoff_release_does_not_cancel_an_agent_source() {
        let mut coordinator = DragDropCoordinator::new(Role::Agent);
        coordinator.begin_local_drag(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

        let effects = coordinator.native_drop_released(Uuid::from_u128(1));

        assert!(coordinator.active_session().is_some());
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, DragEffect::Send(WireMessage::DragDropCancel { .. }))));
    }
}
