use crate::{
    tcp_transport::{TcpFramedReader, TcpFramedTransport, TcpFramedWriter},
    transport::{ConnectionCommand, ConnectionEvent, TcpConnectionSettings},
};
use borderless_core::protocol::WireMessage;
use std::future::Future;
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::{sleep, Duration},
};

const RELIABLE_READ_TIMEOUT: Duration = Duration::from_secs(5);

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
    settings: TcpConnectionSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);

    loop {
        let peer = settings.peer_addr();
        emit(&events, ConnectionEvent::Connecting(peer.clone()));

        let wait_after_disconnect =
            match connect_or_stop(TcpFramedTransport::connect(&peer), &mut commands).await {
                ConnectAttempt::Connected(transport) => {
                    emit(&events, ConnectionEvent::Connected { peer: peer.clone() });
                    if run_tcp_connection(transport, &events, &mut commands).await? {
                        return Ok(());
                    }
                    true
                }
                ConnectAttempt::Failed(error) => {
                    emit(
                        &events,
                        ConnectionEvent::Error(connection_failure_message(&peer, &error)),
                    );
                    if wait_before_reconnect(&mut commands).await {
                        return Ok(());
                    }
                    false
                }
                ConnectAttempt::Stopped => return Ok(()),
            };

        emit(&events, ConnectionEvent::Disconnected(peer));
        if wait_after_disconnect && wait_before_reconnect(&mut commands).await {
            return Ok(());
        }
    }
}

fn connection_failure_message(peer: &str, error: &str) -> String {
    format!(
        "connection to {peer} failed: {error}. Check that Borderless is running as Agent on the target computer, the IP/port match, and Windows firewall allows the configured port."
    )
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
                    Some(ConnectionCommand::Send(_)) => {}
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
                    Some(ConnectionCommand::Send(message)) => {
                        if driver_tx.send(ReliableDriverCommand::Send(message)).is_err() {
                            emit(events, ConnectionEvent::Error("TCP control channel closed".to_string()));
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

async fn wait_before_reconnect(commands: &mut UnboundedReceiver<ConnectionCommand>) -> bool {
    let delay = sleep(Duration::from_millis(500));
    tokio::pin!(delay);

    loop {
        tokio::select! {
            _ = &mut delay => return false,
            command = commands.recv() => {
                match command {
                    Some(ConnectionCommand::Stop) | None => return true,
                    Some(ConnectionCommand::Send(_)) => {}
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
            match tokio::time::timeout(RELIABLE_READ_TIMEOUT, reader.read_frame()).await {
                Ok(Ok(frame)) => {
                    if reader_events
                        .send(ReliableDriverEvent::Frame(frame.message))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Err(err)) => {
                    let _ = reader_events.send(ReliableDriverEvent::Error(err.to_string()));
                    break;
                }
                Err(_) => {
                    let _ = reader_events.send(ReliableDriverEvent::Error(format!(
                        "reliable read timed out after {}ms",
                        RELIABLE_READ_TIMEOUT.as_millis()
                    )));
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

fn emit(events: &UnboundedSender<ConnectionEvent>, event: ConnectionEvent) {
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::input_event::{InputEvent, MouseMoveAbsEvent};
    use tokio::time::{timeout, Duration};
    use tokio::{net::TcpListener, sync::mpsc};

    #[tokio::test]
    async fn pointer_input_is_delivered_on_the_control_tcp_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut transport = TcpFramedTransport::new(stream).unwrap();
            transport.read_frame().await.unwrap().message
        });
        let client = tokio::spawn(run_controller_client(
            TcpConnectionSettings {
                host: "127.0.0.1".to_string(),
                port,
            },
            event_tx,
            command_rx,
        ));

        while !matches!(
            event_rx.recv().await,
            Some(ConnectionEvent::Connected { .. })
        ) {}
        command_tx
            .send(ConnectionCommand::Send(WireMessage::Input(
                InputEvent::MouseMoveAbs(MouseMoveAbsEvent { x: 400, y: 300 }),
            )))
            .unwrap();

        assert!(matches!(
            server.await.unwrap(),
            WireMessage::Input(InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
                x: 400,
                y: 300
            }))
        ));
        command_tx.send(ConnectionCommand::Stop).unwrap();
        client.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reconnect_delay_ignores_send_commands_until_delay_expires() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx
            .send(ConnectionCommand::Send(WireMessage::ReleaseAll))
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
            .send(ConnectionCommand::Send(WireMessage::ReleaseAll))
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
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let settings = TcpConnectionSettings {
            host: "127.0.0.1".to_string(),
            port,
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
    async fn controller_reports_disconnect_when_reliable_channel_is_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let settings = TcpConnectionSettings {
            host: "127.0.0.1".to_string(),
            port,
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let controller = tokio::spawn(run_controller_client(settings, event_tx, command_rx));

        wait_for_connected(&mut event_rx).await;
        timeout(
            RELIABLE_READ_TIMEOUT + Duration::from_secs(1),
            wait_for_disconnected(&mut event_rx),
        )
        .await
        .expect("controller should report disconnected after reliable idle timeout");

        command_tx.send(ConnectionCommand::Stop).unwrap();
        controller.await.unwrap().unwrap();
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn connection_failure_message_names_target_and_setup_checks() {
        let message = connection_failure_message(
            "192.168.1.2:24800",
            "由于目标计算机积极拒绝，无法连接。 (os error 10061)",
        );

        assert!(message.contains("192.168.1.2:24800"));
        assert!(message.contains("Agent"));
        assert!(message.contains("firewall"));
        assert!(message.contains("port"));
        assert!(message.contains("os error 10061"));
    }

    #[test]
    fn reliable_read_timeout_allows_drag_handoff_and_bulk_transfer_jitter() {
        assert!(RELIABLE_READ_TIMEOUT >= Duration::from_secs(5));
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
            if let ConnectionEvent::Disconnected(_) = timeout(
                RELIABLE_READ_TIMEOUT + Duration::from_secs(1),
                event_rx.recv(),
            )
            .await
            .unwrap()
            .unwrap()
            {
                return;
            }
        }
    }

    async fn wait_for_connecting(event_rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) {
        loop {
            if let ConnectionEvent::Connecting(_) = timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                return;
            }
        }
    }
}
