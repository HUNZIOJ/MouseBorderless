use crate::{
    kcp_transport::{KcpFramedTransport, KcpListener},
    latest_pointer::{send_pointer, LatestPointerState, PointerPacket},
    tcp_transport::TcpFramedTransport,
    transport::{ConnectionCommand, ConnectionEvent, TransportSettings},
};
use borderless_core::{
    config::TransportMode,
    input_event::{InputEvent, MouseMoveAbsEvent},
    protocol::WireMessage,
};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};

pub async fn run_agent_server(
    settings: TransportSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);

    match settings.mode {
        TransportMode::Tcp => run_tcp_server(settings, events, &mut commands).await,
        TransportMode::Kcp => run_kcp_server(settings, events, &mut commands).await,
    }
}

async fn run_tcp_server(
    settings: TransportSettings,
    events: UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(settings.peer_addr()).await?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let transport = TcpFramedTransport::new(stream)?;
                emit(
                    &events,
                    ConnectionEvent::Connected {
                        peer: peer.to_string(),
                        mode: settings.mode,
                    },
                );
                if run_tcp_connection(transport, &events, commands).await? {
                    return Ok(());
                }
                emit(&events, ConnectionEvent::Disconnected(peer.to_string()));
                emit(&events, ConnectionEvent::Waiting);
            }
            command = commands.recv() => {
                if matches!(command, Some(ConnectionCommand::Stop) | None) {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_kcp_server(
    settings: TransportSettings,
    events: UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    let listener = KcpFramedTransport::bind(&settings.peer_addr()).await?;

    loop {
        tokio::select! {
            accepted = KcpFramedTransport::accept(&listener) => {
                let transport = accepted?;
                let peer = listener_addr(&listener);
                emit(
                    &events,
                    ConnectionEvent::Connected {
                        peer: peer.clone(),
                        mode: settings.mode,
                    },
                );
                if run_kcp_connection(transport, &settings, &events, commands).await? {
                    return Ok(());
                }
                emit(&events, ConnectionEvent::Disconnected(peer));
                emit(&events, ConnectionEvent::Waiting);
            }
            command = commands.recv() => {
                if matches!(command, Some(ConnectionCommand::Stop) | None) {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_tcp_connection(
    mut transport: TcpFramedTransport,
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<bool> {
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::SendReliable(message)) => {
                        if let Err(err) = transport.send(&message).await {
                            emit(events, ConnectionEvent::Error(err.to_string()));
                            return Ok(false);
                        }
                    }
                    Some(ConnectionCommand::SendLatestPointer { x, y }) => {
                        let message = latest_pointer_as_reliable(x, y);
                        if let Err(err) = transport.send(&message).await {
                            emit(events, ConnectionEvent::Error(err.to_string()));
                            return Ok(false);
                        }
                    }
                    Some(ConnectionCommand::Stop) | None => return Ok(true),
                }
            }
            frame = transport.read_frame() => {
                match frame {
                    Ok(frame) => emit(events, ConnectionEvent::Message(frame.message)),
                    Err(err) => {
                        emit(events, ConnectionEvent::Error(err.to_string()));
                        return Ok(false);
                    }
                }
            }
        }
    }
}

async fn run_kcp_connection(
    mut transport: KcpFramedTransport,
    settings: &TransportSettings,
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<bool> {
    let pointer_socket = UdpSocket::bind(settings.pointer_addr()).await?;
    let pointer_target = settings.pointer_addr();
    let mut pointer_state = LatestPointerState::default();
    let mut pointer_sequence = 1;
    let mut pointer_buf = [0u8; 64];

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::SendReliable(message)) => {
                        if let Err(err) = transport.send(&message).await {
                            emit(events, ConnectionEvent::Error(err.to_string()));
                            return Ok(false);
                        }
                    }
                    Some(ConnectionCommand::SendLatestPointer { x, y }) => {
                        let packet = PointerPacket {
                            sequence: pointer_sequence,
                            x,
                            y,
                        };
                        pointer_sequence += 1;
                        if let Err(err) = send_pointer(&pointer_socket, &pointer_target, packet).await {
                            emit(events, ConnectionEvent::Error(err.to_string()));
                        }
                    }
                    Some(ConnectionCommand::Stop) | None => return Ok(true),
                }
            }
            received = pointer_socket.recv_from(&mut pointer_buf) => {
                match received {
                    Ok((len, _)) => match PointerPacket::decode(&pointer_buf[..len]) {
                        Ok(packet) => {
                            if let Some((x, y)) = pointer_state.accept(packet) {
                                emit(
                                    events,
                                    ConnectionEvent::LatestPointer {
                                        x,
                                        y,
                                        sequence: packet.sequence,
                                    },
                                );
                            }
                        }
                        Err(err) => emit(events, ConnectionEvent::Error(err.to_string())),
                    },
                    Err(err) => emit(events, ConnectionEvent::Error(err.to_string())),
                }
            }
            frame = transport.read_frame() => {
                match frame {
                    Ok(frame) => emit(events, ConnectionEvent::Message(frame.message)),
                    Err(err) => {
                        emit(events, ConnectionEvent::Error(err.to_string()));
                        return Ok(false);
                    }
                }
            }
        }
    }
}

fn latest_pointer_as_reliable(x: i32, y: i32) -> WireMessage {
    WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x, y }))
}

fn listener_addr(listener: &KcpListener) -> String {
    listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn emit(events: &UnboundedSender<ConnectionEvent>, event: ConnectionEvent) {
    let _ = events.send(event);
}
