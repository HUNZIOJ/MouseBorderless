use crate::{
    kcp_transport::{KcpFramedReader, KcpFramedTransport, KcpFramedWriter},
    latest_pointer::{send_pointer, source_matches_peer, LatestPointerState, PointerPacket},
    tcp_transport::{TcpFramedReader, TcpFramedTransport, TcpFramedWriter},
    transport::{ConnectionCommand, ConnectionEvent, TransportSettings},
};
use borderless_core::{
    config::TransportMode,
    input_event::{InputEvent, MouseMoveAbsEvent},
    protocol::WireMessage,
};
use std::net::SocketAddr;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::{sleep, Duration},
};

enum ReliableDriverCommand {
    Send(WireMessage),
    Stop,
}

enum ReliableDriverEvent {
    Frame(WireMessage),
    Error(String),
}

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
    emit(&events, ConnectionEvent::Connecting(settings.peer_addr()));
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
                if wait_after_disconnect_backoff(commands, Duration::from_millis(500)).await {
                    return Ok(());
                }
                emit(&events, ConnectionEvent::Waiting);
                emit(&events, ConnectionEvent::Connecting(settings.peer_addr()));
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
    emit(&events, ConnectionEvent::Connecting(settings.peer_addr()));
    let listener = KcpFramedTransport::bind(&settings.peer_addr()).await?;

    loop {
        tokio::select! {
            accepted = KcpFramedTransport::accept(&listener) => {
                let accepted = accepted?;
                let peer = accepted.peer;
                let peer_display = peer.to_string();
                emit(
                    &events,
                    ConnectionEvent::Connected {
                        peer: peer_display.clone(),
                        mode: settings.mode,
                    },
                );
                if run_kcp_connection(accepted.transport, peer, &settings, &events, commands).await? {
                    return Ok(());
                }
                emit(&events, ConnectionEvent::Disconnected(peer_display));
                if wait_after_disconnect_backoff(commands, Duration::from_millis(500)).await {
                    return Ok(());
                }
                emit(&events, ConnectionEvent::Waiting);
                emit(&events, ConnectionEvent::Connecting(settings.peer_addr()));
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
    transport: TcpFramedTransport,
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<bool> {
    let (driver_tx, driver, mut driver_events) = spawn_tcp_driver(transport);

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::SendReliable(message)) => {
                        if driver_tx.send(ReliableDriverCommand::Send(message)).is_err() {
                            emit(events, ConnectionEvent::Error("reliable transport closed".to_string()));
                            return Ok(false);
                        }
                    }
                    Some(ConnectionCommand::SendLatestPointer { x, y }) => {
                        let message = latest_pointer_as_reliable(x, y);
                        if driver_tx.send(ReliableDriverCommand::Send(message)).is_err() {
                            emit(events, ConnectionEvent::Error("reliable transport closed".to_string()));
                            return Ok(false);
                        }
                    }
                    Some(ConnectionCommand::Stop) | None => {
                        stop_driver(driver_tx, driver).await;
                        return Ok(true);
                    }
                }
            }
            driver_event = driver_events.recv() => {
                match driver_event {
                    Some(ReliableDriverEvent::Frame(message)) => {
                        emit(events, ConnectionEvent::Message(message));
                    }
                    Some(ReliableDriverEvent::Error(error)) => {
                        emit(events, ConnectionEvent::Error(error));
                        stop_driver(driver_tx, driver).await;
                        return Ok(false);
                    }
                    None => return Ok(false),
                }
            }
        }
    }
}

async fn run_kcp_connection(
    transport: KcpFramedTransport,
    peer: SocketAddr,
    settings: &TransportSettings,
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<bool> {
    let (driver_tx, driver, mut driver_events) = spawn_kcp_driver(transport);
    let pointer_socket = UdpSocket::bind(settings.pointer_addr()?).await?;
    let pointer_target = SocketAddr::new(peer.ip(), settings.pointer_port).to_string();
    let mut pointer_state = LatestPointerState::default();
    let mut pointer_sequence = 1;
    let mut pointer_buf = [0u8; 64];

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::SendReliable(message)) => {
                        if driver_tx.send(ReliableDriverCommand::Send(message)).is_err() {
                            emit(events, ConnectionEvent::Error("reliable transport closed".to_string()));
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
                    Some(ConnectionCommand::Stop) | None => {
                        stop_driver(driver_tx, driver).await;
                        return Ok(true);
                    }
                }
            }
            received = pointer_socket.recv_from(&mut pointer_buf) => {
                match received {
                    Ok((len, source)) => {
                        if source_matches_peer(source, peer) {
                            match PointerPacket::decode(&pointer_buf[..len]) {
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
                            }
                        }
                    }
                    Err(err) => emit(events, ConnectionEvent::Error(err.to_string())),
                }
            }
            driver_event = driver_events.recv() => {
                match driver_event {
                    Some(ReliableDriverEvent::Frame(message)) => {
                        emit(events, ConnectionEvent::Message(message));
                    }
                    Some(ReliableDriverEvent::Error(error)) => {
                        emit(events, ConnectionEvent::Error(error));
                        stop_driver(driver_tx, driver).await;
                        return Ok(false);
                    }
                    None => return Ok(false),
                }
            }
        }
    }
}

fn latest_pointer_as_reliable(x: i32, y: i32) -> WireMessage {
    WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x, y }))
}

async fn wait_after_disconnect_backoff(
    commands: &mut UnboundedReceiver<ConnectionCommand>,
    delay: Duration,
) -> bool {
    let delay = sleep(delay);
    tokio::pin!(delay);

    loop {
        tokio::select! {
            _ = &mut delay => return false,
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::Stop) | None => return true,
                    Some(ConnectionCommand::SendReliable(_))
                    | Some(ConnectionCommand::SendLatestPointer { .. }) => {}
                }
            }
        }
    }
}

fn spawn_tcp_driver(
    transport: TcpFramedTransport,
) -> (
    UnboundedSender<ReliableDriverCommand>,
    JoinHandle<()>,
    UnboundedReceiver<ReliableDriverEvent>,
) {
    let (reader, writer) = transport.split();
    spawn_driver(reader, writer)
}

fn spawn_kcp_driver(
    transport: KcpFramedTransport,
) -> (
    UnboundedSender<ReliableDriverCommand>,
    JoinHandle<()>,
    UnboundedReceiver<ReliableDriverEvent>,
) {
    let (reader, writer) = transport.split();
    spawn_driver(reader, writer)
}

fn spawn_driver<R, W>(
    mut reader: R,
    mut writer: W,
) -> (
    UnboundedSender<ReliableDriverCommand>,
    JoinHandle<()>,
    UnboundedReceiver<ReliableDriverEvent>,
)
where
    R: ReliableReader + Send + 'static,
    W: ReliableWriter + Send + 'static,
{
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader_events = event_tx.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match reader.read_frame().await {
                Ok(frame) => {
                    if reader_events
                        .send(ReliableDriverEvent::Frame(frame.message))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = reader_events.send(ReliableDriverEvent::Error(err.to_string()));
                    break;
                }
            }
        }
    });
    let driver = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                ReliableDriverCommand::Send(message) => {
                    if let Err(err) = writer.send(&message).await {
                        let _ = event_tx.send(ReliableDriverEvent::Error(err.to_string()));
                        break;
                    }
                }
                ReliableDriverCommand::Stop => break,
            }
        }
        reader_task.abort();
        let _ = reader_task.await;
    });

    (command_tx, driver, event_rx)
}

async fn stop_driver(driver_tx: UnboundedSender<ReliableDriverCommand>, driver: JoinHandle<()>) {
    let _ = driver_tx.send(ReliableDriverCommand::Stop);
    let _ = driver.await;
}

trait ReliableReader {
    fn read_frame(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<borderless_core::protocol::DecodedFrame>>
           + Send
           + '_;
}

impl ReliableReader for TcpFramedReader {
    fn read_frame(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<borderless_core::protocol::DecodedFrame>>
           + Send
           + '_ {
        TcpFramedReader::read_frame(self)
    }
}

impl ReliableReader for KcpFramedReader {
    fn read_frame(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<borderless_core::protocol::DecodedFrame>>
           + Send
           + '_ {
        KcpFramedReader::read_frame(self)
    }
}

trait ReliableWriter {
    fn send<'a>(
        &'a mut self,
        message: &'a WireMessage,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a;
}

impl ReliableWriter for TcpFramedWriter {
    fn send<'a>(
        &'a mut self,
        message: &'a WireMessage,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        TcpFramedWriter::send(self, message)
    }
}

impl ReliableWriter for KcpFramedWriter {
    fn send<'a>(
        &'a mut self,
        message: &'a WireMessage,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        KcpFramedWriter::send(self, message)
    }
}

fn emit(events: &UnboundedSender<ConnectionEvent>, event: ConnectionEvent) {
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn post_disconnect_backoff_ignores_send_commands_until_delay_expires() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx
            .send(ConnectionCommand::SendLatestPointer { x: 1, y: 2 })
            .unwrap();

        let delayed = timeout(
            Duration::from_millis(100),
            wait_after_disconnect_backoff(&mut command_rx, Duration::from_millis(500)),
        )
        .await;

        assert!(
            delayed.is_err(),
            "send command ended post-disconnect backoff before the delay"
        );

        command_tx.send(ConnectionCommand::Stop).unwrap();
        assert!(
            timeout(
                Duration::from_millis(100),
                wait_after_disconnect_backoff(&mut command_rx, Duration::from_millis(500)),
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn post_disconnect_backoff_stops_immediately_on_stop() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx.send(ConnectionCommand::Stop).unwrap();

        assert!(
            timeout(
                Duration::from_millis(100),
                wait_after_disconnect_backoff(&mut command_rx, Duration::from_millis(500)),
            )
            .await
            .unwrap()
        );
    }
}
