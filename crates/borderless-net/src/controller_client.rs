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
use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
use tokio::{
    net::UdpSocket,
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

enum ConnectAttempt<T> {
    Connected(T),
    Failed(String),
    Stopped,
}

pub async fn run_controller_client(
    settings: TransportSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);
    let mut pointer_session = LatestPointerSession::default();

    loop {
        let peer = settings.peer_addr();
        emit(&events, ConnectionEvent::Connecting(peer.clone()));

        let wait_after_disconnect = match settings.mode {
            TransportMode::Tcp => {
                match connect_or_stop(TcpFramedTransport::connect(&peer), &mut commands).await {
                    ConnectAttempt::Connected(transport) => {
                        emit(
                            &events,
                            ConnectionEvent::Connected {
                                peer: peer.clone(),
                                mode: settings.mode,
                            },
                        );
                        if run_tcp_connection(transport, &events, &mut commands).await? {
                            return Ok(());
                        }
                        true
                    }
                    ConnectAttempt::Failed(error) => {
                        emit(&events, ConnectionEvent::Error(error));
                        if wait_before_reconnect(&mut commands).await {
                            return Ok(());
                        }
                        false
                    }
                    ConnectAttempt::Stopped => return Ok(()),
                }
            }
            TransportMode::Kcp => {
                match connect_or_stop(KcpFramedTransport::connect(&peer), &mut commands).await {
                    ConnectAttempt::Connected(transport) => {
                        emit(
                            &events,
                            ConnectionEvent::Connected {
                                peer: peer.clone(),
                                mode: settings.mode,
                            },
                        );
                        if run_kcp_connection(
                            transport,
                            &settings,
                            &events,
                            &mut commands,
                            &mut pointer_session,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        true
                    }
                    ConnectAttempt::Failed(error) => {
                        emit(&events, ConnectionEvent::Error(error));
                        if wait_before_reconnect(&mut commands).await {
                            return Ok(());
                        }
                        false
                    }
                    ConnectAttempt::Stopped => return Ok(()),
                }
            }
        };

        emit(&events, ConnectionEvent::Disconnected(peer));
        if wait_after_disconnect && wait_before_reconnect(&mut commands).await {
            return Ok(());
        }
    }
}

async fn connect_or_stop<T, F>(
    connect: F,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
) -> ConnectAttempt<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::pin!(connect);

    loop {
        tokio::select! {
            result = &mut connect => {
                return match result {
                    Ok(transport) => ConnectAttempt::Connected(transport),
                    Err(err) => ConnectAttempt::Failed(err.to_string()),
                };
            }
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::Stop) | None => return ConnectAttempt::Stopped,
                    Some(ConnectionCommand::SendReliable(_))
                    | Some(ConnectionCommand::SendLatestPointer { .. }) => {}
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
    settings: &TransportSettings,
    events: &UnboundedSender<ConnectionEvent>,
    commands: &mut UnboundedReceiver<ConnectionCommand>,
    pointer_session: &mut LatestPointerSession,
) -> anyhow::Result<bool> {
    let pointer_socket = UdpSocket::bind(pointer_bind_addr(settings)?).await?;
    let pointer_peer = settings.pointer_addr()?;
    let pointer_target = settings.pointer_addr_string();
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
            received = pointer_socket.recv_from(&mut pointer_buf) => {
                match received {
                    Ok((len, source)) => {
                        if source_matches_peer(source, pointer_peer) {
                            match PointerPacket::decode(&pointer_buf[..len]) {
                                Ok(packet) => {
                                    if let Some((x, y)) = pointer_session.accept(packet) {
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
        }
    }
}

async fn wait_before_reconnect(commands: &mut UnboundedReceiver<ConnectionCommand>) -> bool {
    let delay = sleep(Duration::from_millis(500));
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

fn latest_pointer_as_reliable(x: i32, y: i32) -> WireMessage {
    WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x, y }))
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

fn pointer_bind_addr(settings: &TransportSettings) -> anyhow::Result<SocketAddr> {
    let ip = match settings.reliable_addr()?.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    Ok(SocketAddr::new(ip, settings.pointer_port))
}

fn emit(events: &UnboundedSender<ConnectionEvent>, event: ConnectionEvent) {
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{kcp_transport::KcpFramedTransport, latest_pointer::send_pointer};
    use borderless_core::config::TransportMode;
    use tokio::time::{timeout, Duration};
    use tokio::{net::TcpListener, sync::mpsc};

    #[tokio::test]
    async fn reconnect_delay_ignores_send_commands_until_delay_expires() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx
            .send(ConnectionCommand::SendLatestPointer { x: 1, y: 2 })
            .unwrap();

        let delayed = timeout(
            Duration::from_millis(100),
            wait_before_reconnect(&mut command_rx),
        )
        .await;

        assert!(
            delayed.is_err(),
            "send command ended reconnect delay before the 500ms backoff"
        );

        command_tx.send(ConnectionCommand::Stop).unwrap();
        assert!(wait_before_reconnect(&mut command_rx).await);
    }

    #[tokio::test]
    async fn connect_or_stop_stops_while_connect_is_pending() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        command_tx.send(ConnectionCommand::Stop).unwrap();

        let result = timeout(
            Duration::from_millis(100),
            connect_or_stop(
                std::future::pending::<anyhow::Result<()>>(),
                &mut command_rx,
            ),
        )
        .await
        .unwrap();

        assert!(matches!(result, ConnectAttempt::Stopped));
    }

    #[tokio::test]
    async fn connect_or_stop_ignores_send_commands_while_connect_is_pending() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        command_tx
            .send(ConnectionCommand::SendLatestPointer { x: 1, y: 2 })
            .unwrap();

        let delayed = timeout(
            Duration::from_millis(100),
            connect_or_stop(
                std::future::pending::<anyhow::Result<()>>(),
                &mut command_rx,
            ),
        )
        .await;

        assert!(
            delayed.is_err(),
            "send command interrupted the pending connect attempt"
        );
    }

    #[tokio::test]
    async fn controller_waits_after_live_disconnect_before_reconnecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reliable_port = listener.local_addr().unwrap().port();
        let pointer_port = unused_udp_port().await;
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let settings = TransportSettings {
            mode: TransportMode::Tcp,
            host: "127.0.0.1".to_string(),
            reliable_port,
            pointer_port,
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let controller = tokio::spawn(run_controller_client(settings, event_tx, command_rx));

        wait_for_connected(&mut event_rx).await;
        wait_for_disconnected(&mut event_rx).await;

        let reconnect = timeout(
            Duration::from_millis(100),
            wait_for_connecting(&mut event_rx),
        )
        .await;
        assert!(
            reconnect.is_err(),
            "controller attempted reconnect without live-disconnect backoff"
        );

        command_tx.send(ConnectionCommand::Stop).unwrap();
        controller.await.unwrap().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn kcp_controller_receives_fresh_latest_pointer_packets_on_configured_port() {
        let listener = KcpFramedTransport::bind("127.0.0.2:0").await.unwrap();
        let reliable_port = listener.local_addr().unwrap().port();
        let pointer_port = unused_udp_port().await;
        let _server = tokio::spawn(async move {
            let _accepted = KcpFramedTransport::accept(&listener).await.unwrap();
            std::future::pending::<()>().await;
        });

        let settings = TransportSettings {
            mode: TransportMode::Kcp,
            host: "127.0.0.2".to_string(),
            reliable_port,
            pointer_port,
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let controller = tokio::spawn(run_controller_client(settings, event_tx, command_rx));

        wait_for_connected(&mut event_rx).await;

        let sender = UdpSocket::bind(format!("127.0.0.2:{pointer_port}"))
            .await
            .unwrap();
        let target = format!("127.0.0.1:{pointer_port}");
        for packet in [
            PointerPacket {
                sequence: 10,
                x: 100,
                y: 200,
            },
            PointerPacket {
                sequence: 9,
                x: 300,
                y: 400,
            },
            PointerPacket {
                sequence: 11,
                x: 500,
                y: 600,
            },
        ] {
            send_pointer(&sender, &target, packet).await.unwrap();
        }

        assert_eq!(
            next_latest_pointer(&mut event_rx).await,
            ConnectionEvent::LatestPointer {
                x: 100,
                y: 200,
                sequence: 10,
            }
        );
        assert_eq!(
            next_latest_pointer(&mut event_rx).await,
            ConnectionEvent::LatestPointer {
                x: 500,
                y: 600,
                sequence: 11,
            }
        );

        command_tx.send(ConnectionCommand::Stop).unwrap();
        controller.await.unwrap().unwrap();
    }

    async fn unused_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn wait_for_connected(event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) {
        loop {
            match timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ConnectionEvent::Connected { .. } => return,
                ConnectionEvent::Error(error) => panic!("unexpected connection error: {error}"),
                _ => {}
            }
        }
    }

    async fn wait_for_disconnected(event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) {
        loop {
            match timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ConnectionEvent::Disconnected(_) => return,
                _ => {}
            }
        }
    }

    async fn wait_for_connecting(event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) {
        loop {
            match timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ConnectionEvent::Connecting(_) => return,
                _ => {}
            }
        }
    }

    async fn next_latest_pointer(
        event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>,
    ) -> ConnectionEvent {
        loop {
            match timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                event @ ConnectionEvent::LatestPointer { .. } => return event,
                ConnectionEvent::Error(error) => panic!("unexpected connection error: {error}"),
                _ => {}
            }
        }
    }
}
