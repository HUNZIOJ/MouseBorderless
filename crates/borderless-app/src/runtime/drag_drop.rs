use borderless_core::{
    config::{RemotePosition, Role},
    control::ControlMode,
    drag_drop::{DragDirection, DragDropPhase, DragDropSession},
    geometry::{edge_for_position, Edge, Point},
    protocol::WireMessage,
};
use borderless_net::bulk_transfer::BulkTransferEvent;
use std::time::{Duration, Instant};
use uuid::Uuid;

const TARGET_AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragEffect {
    InstallEdge(Edge),
    UninstallEdge,
    Send(WireMessage),
    AwaitLocalRelease {
        session_id: Uuid,
    },
    ResolveTarget {
        session_id: Uuid,
        transfer_id: Uuid,
        point: Point,
    },
    BeginTransfer {
        session_id: Uuid,
        transfer_id: Uuid,
        authorization: Uuid,
        source_paths: Vec<String>,
        destination: String,
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
    transfer_id: Uuid,
    _item_count: u32,
    handed_off: bool,
    released: bool,
}

pub struct DragDropCoordinator {
    role: Role,
    control_mode: ControlMode,
    peer_layout: Option<RemotePosition>,
    active: Option<DragDropSession>,
    peer_offer: Option<PeerDragOffer>,
    active_deadline: Option<Instant>,
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
            active_deadline: None,
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
                    effects.extend(self.terminate_active(
                        "拖拽已返回本机".to_string(),
                        false,
                        true,
                    ));
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
        let target_selecting = self.role == Role::Agent || self.control_mode == ControlMode::Remote;
        if target_selecting {
            let _ = session.begin_remote_target_selection();
        }
        self.active = Some(session);
        self.active_deadline = None;

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
        if target_selecting {
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
        let matches_session = self
            .active
            .as_ref()
            .is_some_and(|session| session.session_id() == session_id);
        if !matches_session {
            return Vec::new();
        }
        self.terminate_active(reason.into(), false, true)
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

    pub fn local_left_released(&mut self, point: Point) -> Vec<DragEffect> {
        if self.role != Role::Controller {
            return Vec::new();
        }

        if self.control_mode == ControlMode::Remote {
            let Some(session) = self.active.as_mut() else {
                return Vec::new();
            };
            if session.phase() != &DragDropPhase::RemoteTargetSelecting {
                return Vec::new();
            }
            let session_id = session.session_id();
            if session.release_at(point).is_err() {
                return Vec::new();
            }
            self.active_deadline = Some(Instant::now() + TARGET_AUTHORIZATION_TIMEOUT);
            return vec![
                DragEffect::Send(WireMessage::DragDropReleased { session_id, point }),
                DragEffect::RestoreLocalInput,
                DragEffect::Status {
                    state: "正在解析目标目录".to_string(),
                    destination: None,
                },
            ];
        }

        let Some(offer) = self.peer_offer.as_mut().filter(|offer| offer.handed_off) else {
            return Vec::new();
        };
        if offer.released {
            return Vec::new();
        }
        offer.released = true;
        vec![
            DragEffect::Send(WireMessage::DragDropReleased {
                session_id: offer.session_id,
                point,
            }),
            DragEffect::ResolveTarget {
                session_id: offer.session_id,
                transfer_id: offer.transfer_id,
                point,
            },
            DragEffect::Status {
                state: "正在解析目标目录".to_string(),
                destination: None,
            },
        ]
    }

    pub fn begin_connection_generation(&mut self, generation: u64) {
        self.connection_generation = generation;
    }

    pub fn handle_tagged_peer_message(
        &mut self,
        generation: u64,
        message: WireMessage,
    ) -> Vec<DragEffect> {
        if generation != self.connection_generation {
            return Vec::new();
        }
        self.handle_peer_message(message)
    }

    pub fn on_disconnect(&mut self, generation: u64) -> Vec<DragEffect> {
        if generation != self.connection_generation {
            return Vec::new();
        }
        let mut effects = self.terminate_active("连接中断".to_string(), true, false);
        if !effects.contains(&DragEffect::Send(WireMessage::ReleaseAll)) {
            effects.push(DragEffect::Send(WireMessage::ReleaseAll));
        }
        effects.push(DragEffect::UninstallEdge);
        effects
    }

    pub fn on_stop(&mut self) -> Vec<DragEffect> {
        let mut effects = self.terminate_active("程序已停止".to_string(), false, true);
        if !effects.contains(&DragEffect::Send(WireMessage::ReleaseAll)) {
            effects.push(DragEffect::Send(WireMessage::ReleaseAll));
        }
        effects.push(DragEffect::UninstallEdge);
        effects
    }

    pub fn tick(&mut self, now: Instant) -> Vec<DragEffect> {
        if self.active_deadline.is_some_and(|deadline| now > deadline) {
            return self.terminate_active("等待目标目录授权超时".to_string(), true, true);
        }
        Vec::new()
    }

    pub fn authorization_expired(&mut self, session_id: Uuid) -> Vec<DragEffect> {
        let matches_offer = self
            .peer_offer
            .as_ref()
            .is_some_and(|offer| offer.session_id == session_id);
        if !matches_offer {
            return Vec::new();
        }
        let mut effects = self.terminate_active("目标目录授权超时".to_string(), true, false);
        effects.insert(
            0,
            DragEffect::Send(WireMessage::DragDropTargetFailed {
                session_id,
                reason: "目标目录授权超时".to_string(),
            }),
        );
        effects
    }

    fn terminate_active(
        &mut self,
        reason: String,
        failed: bool,
        notify_peer: bool,
    ) -> Vec<DragEffect> {
        let source = self.active.take();
        let target = self.peer_offer.take();
        self.active_deadline = None;

        let (session_id, transfer_id) = if let Some(mut session) = source {
            if failed {
                let _ = session.fail(reason.clone());
            } else {
                let _ = session.cancel(reason.clone());
            }
            (session.session_id(), session.transfer_id())
        } else if let Some(offer) = target {
            (offer.session_id, offer.transfer_id)
        } else {
            return vec![DragEffect::RestoreLocalInput];
        };

        let mut effects = vec![
            DragEffect::CancelBulk { transfer_id },
            DragEffect::RevokeAuthorization { session_id },
        ];
        if notify_peer {
            effects.push(DragEffect::Send(WireMessage::DragDropCancel {
                session_id,
                reason: reason.clone(),
            }));
        }
        effects.extend([
            DragEffect::Send(WireMessage::ReleaseAll),
            DragEffect::RestoreLocalInput,
            DragEffect::Status {
                state: if failed {
                    format!("拖放失败：{reason}")
                } else {
                    format!("拖放已取消：{reason}")
                },
                destination: None,
            },
        ]);
        effects
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
                    transfer_id,
                    _item_count: item_count,
                    handed_off: false,
                    released: false,
                });
                self.local_release_handoff_effects()
            }
            WireMessage::DragDropReleased { session_id, point } => {
                if let Some(session) = self
                    .active
                    .as_mut()
                    .filter(|session| session.session_id() == session_id)
                {
                    if session.phase() == &DragDropPhase::RemoteTargetSelecting
                        && session.release_at(point).is_ok()
                    {
                        self.active_deadline = Some(Instant::now() + TARGET_AUTHORIZATION_TIMEOUT);
                        return vec![
                            DragEffect::RestoreLocalInput,
                            DragEffect::Status {
                                state: "正在解析目标目录".to_string(),
                                destination: None,
                            },
                        ];
                    }
                    return Vec::new();
                }

                let Some(offer) = self
                    .peer_offer
                    .as_mut()
                    .filter(|offer| offer.session_id == session_id && !offer.released)
                else {
                    return Vec::new();
                };
                offer.released = true;
                vec![
                    DragEffect::ResolveTarget {
                        session_id,
                        transfer_id: offer.transfer_id,
                        point,
                    },
                    DragEffect::Status {
                        state: "正在解析目标目录".to_string(),
                        destination: None,
                    },
                ]
            }
            WireMessage::DragDropTargetResolved { session_id, target } => {
                let Some(session) = self
                    .active
                    .as_mut()
                    .filter(|session| session.session_id() == session_id)
                else {
                    return Vec::new();
                };
                if session.resolve_target(target.clone()).is_err()
                    || session.begin_transfer().is_err()
                {
                    return Vec::new();
                }
                self.active_deadline = None;
                vec![
                    DragEffect::BeginTransfer {
                        session_id,
                        transfer_id: session.transfer_id(),
                        authorization: target.authorization,
                        source_paths: session.source_paths().to_vec(),
                        destination: target.display_name.clone(),
                    },
                    DragEffect::Status {
                        state: "正在传输到目标目录".to_string(),
                        destination: Some(target.display_name),
                    },
                ]
            }
            WireMessage::DragDropTargetFailed { session_id, reason } => {
                let matches_session = self
                    .active
                    .as_ref()
                    .is_some_and(|session| session.session_id() == session_id);
                if !matches_session {
                    return Vec::new();
                }
                self.terminate_active(reason, true, false)
            }
            WireMessage::DragDropTransferStarted {
                session_id,
                transfer_id,
            } => {
                let matches_offer = self.peer_offer.as_ref().is_some_and(|offer| {
                    offer.session_id == session_id && offer.transfer_id == transfer_id
                });
                if matches_offer {
                    vec![DragEffect::Status {
                        state: "正在接收拖放文件".to_string(),
                        destination: None,
                    }]
                } else {
                    Vec::new()
                }
            }
            WireMessage::DragDropTransferResult {
                session_id,
                transfer_id,
                ok,
                reason,
            } => {
                let matches_session = self.active.as_ref().is_some_and(|session| {
                    session.session_id() == session_id
                        && session.transfer_id() == transfer_id
                        && session.phase() == &DragDropPhase::Transferring
                });
                if !matches_session {
                    return Vec::new();
                }
                if ok {
                    let mut session = self.active.take().expect("matching drag source exists");
                    let _ = session.complete();
                    self.active_deadline = None;
                    vec![
                        DragEffect::RestoreLocalInput,
                        DragEffect::Status {
                            state: "拖放完成".to_string(),
                            destination: None,
                        },
                    ]
                } else {
                    let reason = reason.unwrap_or_else(|| "接收端未能完成传输".to_string());
                    self.terminate_active(reason, true, false)
                }
            }
            WireMessage::DragDropCancel { session_id, reason } => {
                let source_matches = self
                    .active
                    .as_ref()
                    .is_some_and(|session| session.session_id() == session_id);
                let target_matches = self
                    .peer_offer
                    .as_ref()
                    .is_some_and(|offer| offer.session_id == session_id);
                if source_matches || target_matches {
                    self.terminate_active(reason, false, false)
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    pub fn handle_bulk_event(&mut self, event: BulkTransferEvent) -> Vec<DragEffect> {
        let transfer_id = match &event {
            BulkTransferEvent::Offered(manifest) => manifest.transfer_id,
            BulkTransferEvent::Progress { transfer_id, .. }
            | BulkTransferEvent::Sent { transfer_id }
            | BulkTransferEvent::Completed { transfer_id, .. }
            | BulkTransferEvent::Failed { transfer_id, .. } => *transfer_id,
            BulkTransferEvent::Cancelled(transfer_id) => *transfer_id,
        };

        if self
            .active
            .as_ref()
            .is_some_and(|session| session.transfer_id() == transfer_id)
        {
            return match event {
                BulkTransferEvent::Sent { .. } => vec![DragEffect::Status {
                    state: "文件已发送，等待接收端确认".to_string(),
                    destination: None,
                }],
                BulkTransferEvent::Failed { error, .. } => self.terminate_active(error, true, true),
                BulkTransferEvent::Cancelled(_) => {
                    self.terminate_active("本机已取消传输".to_string(), false, true)
                }
                _ => Vec::new(),
            };
        }

        let Some(offer) = self
            .peer_offer
            .as_ref()
            .filter(|offer| offer.transfer_id == transfer_id)
        else {
            return Vec::new();
        };
        let session_id = offer.session_id;
        let result = match event {
            BulkTransferEvent::Completed { .. } => Some((true, None, "拖放完成".to_string())),
            BulkTransferEvent::Failed { error, .. } => {
                Some((false, Some(error.clone()), format!("拖放失败：{error}")))
            }
            BulkTransferEvent::Cancelled(_) => Some((
                false,
                Some("接收端已取消传输".to_string()),
                "拖放已取消".to_string(),
            )),
            _ => None,
        };
        let Some((ok, reason, state)) = result else {
            return Vec::new();
        };
        self.peer_offer = None;
        vec![
            DragEffect::Send(WireMessage::DragDropTransferResult {
                session_id,
                transfer_id,
                ok,
                reason,
            }),
            DragEffect::RevokeAuthorization { session_id },
            DragEffect::RestoreLocalInput,
            DragEffect::Status {
                state,
                destination: None,
            },
        ]
    }

    pub fn complete_target_resolution(
        &mut self,
        session_id: Uuid,
        transfer_id: Uuid,
        result: Result<borderless_core::drag_drop::DropTargetSummary, String>,
    ) -> Vec<DragEffect> {
        let matches_offer = self.peer_offer.as_ref().is_some_and(|offer| {
            offer.session_id == session_id && offer.transfer_id == transfer_id && offer.released
        });
        if !matches_offer {
            return Vec::new();
        }

        match result {
            Ok(target) => vec![
                DragEffect::Send(WireMessage::DragDropTargetResolved {
                    session_id,
                    target: target.clone(),
                }),
                DragEffect::Status {
                    state: "目标目录已确认".to_string(),
                    destination: Some(target.display_name),
                },
            ],
            Err(reason) => {
                let mut effects = self.terminate_active(reason.clone(), true, false);
                effects.insert(
                    0,
                    DragEffect::Send(WireMessage::DragDropTargetFailed { session_id, reason }),
                );
                effects
            }
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
        drag_drop::{DragDirection, DragDropPhase, DropResolutionKind, DropTargetSummary},
        geometry::{Edge, Point},
        protocol::WireMessage,
    };
    use borderless_net::bulk_transfer::BulkTransferEvent;
    use uuid::Uuid;

    fn transferring_source() -> (DragDropCoordinator, Uuid, Uuid) {
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.begin_local_drag(
            session_id,
            transfer_id,
            vec!["C:\\src\\report.pdf".to_string()],
        );
        coordinator.set_control_mode(ControlMode::Remote);
        coordinator.local_left_released(Point::new(20, 30));
        let effects = coordinator.handle_peer_message(WireMessage::DragDropTargetResolved {
            session_id,
            target: DropTargetSummary {
                display_name: "Reports".to_string(),
                resolution_kind: DropResolutionKind::FolderIcon,
                authorization: Uuid::from_u128(3),
            },
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DragEffect::BeginTransfer {
                session_id: candidate_session,
                transfer_id: candidate_transfer,
                ..
            } if *candidate_session == session_id && *candidate_transfer == transfer_id
        )));
        (coordinator, session_id, transfer_id)
    }

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
    fn controller_drag_detected_after_edge_crossing_starts_remote_selection_immediately() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.set_control_mode(ControlMode::Remote);

        let effects = coordinator.begin_local_drag(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

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

    #[test]
    fn peer_release_ends_the_native_drag_for_an_agent_source() {
        let session_id = Uuid::from_u128(1);
        let mut coordinator = DragDropCoordinator::new(Role::Agent);
        coordinator.begin_local_drag(
            session_id,
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );

        let effects = coordinator.handle_peer_message(WireMessage::DragDropReleased {
            session_id,
            point: Point::new(20, 30),
        });

        assert!(effects.contains(&DragEffect::RestoreLocalInput));
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, DragEffect::Send(WireMessage::DragDropCancel { .. }))));
    }

    #[test]
    fn sender_completes_only_after_matching_target_result() {
        let (mut coordinator, session_id, transfer_id) = transferring_source();

        assert!(coordinator
            .handle_bulk_event(BulkTransferEvent::Sent { transfer_id })
            .iter()
            .all(|effect| !matches!(
                effect,
                DragEffect::Status { state, .. } if state == "拖放完成"
            )));

        let effects = coordinator.handle_peer_message(WireMessage::DragDropTransferResult {
            session_id,
            transfer_id,
            ok: true,
            reason: None,
        });
        assert!(effects.contains(&DragEffect::Status {
            state: "拖放完成".to_string(),
            destination: None,
        }));
    }

    #[test]
    fn wrong_transfer_result_is_ignored() {
        let (mut coordinator, session_id, _) = transferring_source();
        let effects = coordinator.handle_peer_message(WireMessage::DragDropTransferResult {
            session_id,
            transfer_id: Uuid::from_u128(999),
            ok: true,
            reason: None,
        });
        assert!(effects.is_empty());
    }

    #[test]
    fn target_resolution_and_bulk_completion_send_verified_result() {
        let session_id = Uuid::from_u128(10);
        let transfer_id = Uuid::from_u128(11);
        let target = DropTargetSummary {
            display_name: "Reports".to_string(),
            resolution_kind: DropResolutionKind::FolderIcon,
            authorization: Uuid::from_u128(12),
        };
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id,
            transfer_id,
            item_count: 1,
        });
        coordinator.local_left_released(Point::new(40, 50));

        let resolved =
            coordinator.complete_target_resolution(session_id, transfer_id, Ok(target.clone()));
        assert!(
            resolved.contains(&DragEffect::Send(WireMessage::DragDropTargetResolved {
                session_id,
                target
            }))
        );

        coordinator.handle_peer_message(WireMessage::DragDropTransferStarted {
            session_id,
            transfer_id,
        });
        let completed = coordinator.handle_bulk_event(BulkTransferEvent::Completed {
            transfer_id,
            cache_paths: vec!["C:\\drop\\report.pdf".to_string()],
        });
        assert!(
            completed.contains(&DragEffect::Send(WireMessage::DragDropTransferResult {
                session_id,
                transfer_id,
                ok: true,
                reason: None,
            }))
        );
    }

    #[test]
    fn failed_target_resolution_clears_the_peer_offer() {
        let session_id = Uuid::from_u128(10);
        let transfer_id = Uuid::from_u128(11);
        let mut coordinator = DragDropCoordinator::new(Role::Agent);
        coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id,
            transfer_id,
            item_count: 1,
        });
        coordinator.handle_peer_message(WireMessage::DragDropReleased {
            session_id,
            point: Point::new(40, 50),
        });

        let effects = coordinator.complete_target_resolution(
            session_id,
            transfer_id,
            Err("目标目录不可写".to_string()),
        );

        assert!(
            effects.contains(&DragEffect::Send(WireMessage::DragDropTargetFailed {
                session_id,
                reason: "目标目录不可写".to_string(),
            }))
        );
        let next = coordinator.handle_peer_message(WireMessage::DragDropEntered {
            session_id: Uuid::from_u128(20),
            transfer_id: Uuid::from_u128(21),
            item_count: 1,
        });
        assert!(next
            .iter()
            .all(|effect| !matches!(effect, DragEffect::Send(WireMessage::DragDropCancel { .. }))));
    }

    #[test]
    fn disconnect_cancels_drag_bulk_and_releases_input() {
        let (mut coordinator, _session_id, transfer_id) = transferring_source();
        coordinator.begin_connection_generation(7);

        let effects = coordinator.on_disconnect(7);

        assert!(effects.contains(&DragEffect::CancelBulk { transfer_id }));
        assert!(effects.contains(&DragEffect::Send(WireMessage::ReleaseAll)));
        assert!(effects.contains(&DragEffect::RestoreLocalInput));
        assert!(effects.contains(&DragEffect::UninstallEdge));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DragEffect::Status { state, .. } if state.contains("连接中断")
        )));
        assert!(coordinator.active_session().is_none());
    }

    #[test]
    fn stale_connection_generation_messages_are_ignored() {
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.begin_connection_generation(8);

        let effects = coordinator.handle_tagged_peer_message(
            7,
            WireMessage::DragDropCancel {
                session_id: Uuid::new_v4(),
                reason: "old connection".to_string(),
            },
        );

        assert!(effects.is_empty());
    }

    #[test]
    fn target_authorization_timeout_fails_without_starting_bulk() {
        let session_id = Uuid::from_u128(1);
        let mut coordinator = DragDropCoordinator::new(Role::Controller);
        coordinator.begin_local_drag(
            session_id,
            Uuid::from_u128(2),
            vec!["C:\\src\\report.pdf".to_string()],
        );
        coordinator.set_control_mode(ControlMode::Remote);
        coordinator.local_left_released(Point::new(20, 30));

        let effects = coordinator.tick(Instant::now() + Duration::from_secs(31));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            DragEffect::Status { state, .. } if state.contains("超时")
        )));
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, DragEffect::BeginTransfer { .. })));
        assert!(coordinator.active_session().is_none());
    }
}
