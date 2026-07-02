use crate::{
    kcp_transport::{KcpFramedReader, KcpFramedTransport, KcpFramedWriter},
    latest_pointer::{send_pointer, source_matches_peer, LatestPointerSession, PointerPacket},
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

struct KcpPointerEndpoint {
    socket: UdpSocket,
    peer: SocketAddr,
    target: String,
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
    let mut pointer_session = LatestPointerSession::default();

    loop {
        tokio::select! {
            accepted = KcpFramedTransport::accept(&listener) => {
                let accepted = accepted?;
                let peer = accepted.peer;
                let peer_display = peer.to_string();
                let pointer_endpoint = match prepare_kcp_pointer_endpoint(peer, &settings).await {
                    Ok(endpoint) => endpoint,
                    Err(err) => {
                        emit(&events, ConnectionEvent::Error(err.to_string()));
                        emit(&events, ConnectionEvent::Disconnected(peer_display));
                        if wait_after_disconnect_backoff(commands, Duration::from_millis(500)).await {
                            return Ok(());
                        }
                        emit(&events, ConnectionEvent::Waiting);
                        emit(&events, ConnectionEvent::Connecting(settings.peer_addr()));
                        continue;
                    }
                };
                pointer_session.begin_reliable_session();
                emit(
                    &events,
                    ConnectionEvent::Connected {
                        peer: peer_display.clone(),
                        mode: settings.mode,
                    },
                );
                if run_kcp_connection(
                    accepted.transport,
                    &events,
                    commands,
                    &mut pointer_session,
                    pointer_endpoint,
                )
                .await?
                {
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
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
    pointer_session: &mut LatestPointerSession,
    pointer_endpoint: KcpPointerEndpoint,
) -> anyhow::Result<bool> {
    let pointer_socket = pointer_endpoint.socket;
    let pointer_peer = pointer_endpoint.peer;
    let pointer_target = pointer_endpoint.target;
    let mut pointer_buf = [0u8; 64];
    let (driver_tx, driver, mut driver_events) = spawn_kcp_driver(transport);

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
                        let packet = pointer_session.next_packet(x, y);
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
                        if source_matches_peer(source, pointer_peer) {
                            match PointerPacket::decode(&pointer_buf[..len]) {
                                Ok(packet) => {
                                    let stale_before = pointer_session.stale_pointer_packets();
                                    if let Some((x, y)) = pointer_session.accept(packet) {
                                        emit(
                                            events,
                                            ConnectionEvent::LatestPointer {
                                                x,
                                                y,
                                                sequence: packet.sequence,
                                            },
                                        );
                                    } else {
                                        let stale_after = pointer_session.stale_pointer_packets();
                                        if stale_after > stale_before {
                                            emit(
                                                events,
                                                ConnectionEvent::StalePointerPackets {
                                                    count: stale_after,
                                                },
                                            );
                                        }
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

fn kcp_pointer_peer(peer: SocketAddr, settings: &TransportSettings) -> SocketAddr {
    SocketAddr::new(peer.ip(), settings.pointer_port)
}

async fn prepare_kcp_pointer_endpoint(
    peer: SocketAddr,
    settings: &TransportSettings,
) -> anyhow::Result<KcpPointerEndpoint> {
    let pointer_peer = kcp_pointer_peer(peer, settings);
    Ok(KcpPointerEndpoint {
        socket: UdpSocket::bind(settings.pointer_addr()?).await?,
        peer: pointer_peer,
        target: pointer_peer.to_string(),
    })
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
    use borderless_core::{
        config::TransportMode,
        geometry::Rect,
        protocol::{Hello, PROTOCOL_VERSION},
    };
    use std::net::{IpAddr, Ipv4Addr};
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
        assert!(timeout(
            Duration::from_millis(100),
            wait_after_disconnect_backoff(&mut command_rx, Duration::from_millis(500)),
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn post_disconnect_backoff_stops_immediately_on_stop() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx.send(ConnectionCommand::Stop).unwrap();

        assert!(timeout(
            Duration::from_millis(100),
            wait_after_disconnect_backoff(&mut command_rx, Duration::from_millis(500)),
        )
        .await
        .unwrap());
    }

    #[test]
    fn kcp_pointer_peer_uses_reliable_peer_ip_and_configured_pointer_port() {
        let settings = TransportSettings {
            mode: TransportMode::Kcp,
            host: "127.0.0.1".to_string(),
            reliable_port: 24800,
            pointer_port: 24801,
        };
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 49152);

        assert_eq!(
            kcp_pointer_peer(peer, &settings),
            SocketAddr::new(peer.ip(), settings.pointer_port)
        );
    }

    #[tokio::test]
    async fn kcp_agent_reports_pointer_setup_error_before_connected() {
        let reliable_listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let reliable_port = reliable_listener.local_addr().unwrap().port();
        drop(reliable_listener);
        let occupied_pointer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let pointer_port = occupied_pointer.local_addr().unwrap().port();

        let settings = TransportSettings {
            mode: TransportMode::Kcp,
            host: "127.0.0.1".to_string(),
            reliable_port,
            pointer_port,
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(run_agent_server(settings, event_tx, command_rx));

        wait_for_connecting(&mut event_rx).await;
        let mut client = KcpFramedTransport::connect(&format!("127.0.0.1:{reliable_port}"))
            .await
            .unwrap();
        client
            .send(&WireMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                desktop: Rect::new(0, 0, 1920, 1080),
            }))
            .await
            .unwrap();

        let first_setup_event = next_connected_or_error(&mut event_rx).await;
        assert!(
            matches!(first_setup_event, ConnectionEvent::Error(_)),
            "expected pointer setup error before Connected, got {first_setup_event:?}"
        );

        command_tx.send(ConnectionCommand::Stop).unwrap();
        agent.await.unwrap().unwrap();
    }

    async fn wait_for_connecting(event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) {
        loop {
            match timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ConnectionEvent::Connecting(_) => return,
                event => {
                    assert!(
                        !matches!(event, ConnectionEvent::Connected { .. }),
                        "Connected arrived before Connecting: {event:?}"
                    );
                }
            }
        }
    }

    async fn next_connected_or_error(
        event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>,
    ) -> ConnectionEvent {
        loop {
            match timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                event @ (ConnectionEvent::Connected { .. } | ConnectionEvent::Error(_)) => {
                    return event
                }
                _ => {}
            }
        }
    }
}
