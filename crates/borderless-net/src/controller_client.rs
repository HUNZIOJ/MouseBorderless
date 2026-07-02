use crate::{
    kcp_transport::KcpFramedTransport,
    latest_pointer::{send_pointer, PointerPacket},
    tcp_transport::TcpFramedTransport,
    transport::{ConnectionCommand, ConnectionEvent, TransportSettings},
};
use borderless_core::{
    config::TransportMode,
    input_event::{InputEvent, MouseMoveAbsEvent},
    protocol::WireMessage,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    time::{sleep, Duration},
};

pub async fn run_controller_client(
    settings: TransportSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);

    loop {
        let peer = settings.peer_addr();
        emit(&events, ConnectionEvent::Connecting(peer.clone()));

        let stopped = match settings.mode {
            TransportMode::Tcp => match TcpFramedTransport::connect(&peer).await {
                Ok(transport) => {
                    emit(
                        &events,
                        ConnectionEvent::Connected {
                            peer: peer.clone(),
                            mode: settings.mode,
                        },
                    );
                    run_tcp_connection(transport, &events, &mut commands).await?
                }
                Err(err) => {
                    emit(&events, ConnectionEvent::Error(err.to_string()));
                    wait_before_reconnect(&mut commands).await
                }
            },
            TransportMode::Kcp => match KcpFramedTransport::connect(&peer).await {
                Ok(transport) => {
                    emit(
                        &events,
                        ConnectionEvent::Connected {
                            peer: peer.clone(),
                            mode: settings.mode,
                        },
                    );
                    run_kcp_connection(transport, &settings, &events, &mut commands).await?
                }
                Err(err) => {
                    emit(&events, ConnectionEvent::Error(err.to_string()));
                    wait_before_reconnect(&mut commands).await
                }
            },
        };

        if stopped {
            return Ok(());
        }

        emit(&events, ConnectionEvent::Disconnected(peer));
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
    let pointer_socket = UdpSocket::bind("0.0.0.0:0").await?;
    let pointer_target = settings.pointer_addr();
    let mut pointer_sequence = 1;

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

async fn wait_before_reconnect(commands: &mut UnboundedReceiver<ConnectionCommand>) -> bool {
    tokio::select! {
        _ = sleep(Duration::from_millis(500)) => false,
        command = commands.recv() => matches!(command, Some(ConnectionCommand::Stop) | None),
    }
}

fn latest_pointer_as_reliable(x: i32, y: i32) -> WireMessage {
    WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x, y }))
}

fn emit(events: &UnboundedSender<ConnectionEvent>, event: ConnectionEvent) {
    let _ = events.send(event);
}
