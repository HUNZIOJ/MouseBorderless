use crate::{
    tcp_transport::{TcpFramedReader, TcpFramedTransport, TcpFramedWriter},
    transport::{ConnectionCommand, ConnectionEvent, TcpConnectionSettings},
};
use borderless_core::protocol::WireMessage;
use tokio::{
    net::TcpListener,
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

pub async fn run_agent_server(
    settings: TcpConnectionSettings,
    events: UnboundedSender<ConnectionEvent>,
    mut commands: UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()> {
    emit(&events, ConnectionEvent::Waiting);

    run_tcp_server(settings, events, &mut commands).await
}

async fn run_tcp_server(
    settings: TcpConnectionSettings,
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
                match command {
                    Some(ConnectionCommand::Stop) | None => return Ok(()),
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
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn post_disconnect_backoff_ignores_send_commands_until_delay_expires() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        command_tx
            .send(ConnectionCommand::Send(WireMessage::ReleaseAll))
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

    #[tokio::test]
    async fn agent_reports_disconnect_when_reliable_channel_is_idle() {
        let reliable_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reliable_listener.local_addr().unwrap().port();
        drop(reliable_listener);

        let settings = TcpConnectionSettings {
            host: "127.0.0.1".to_string(),
            port,
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(run_agent_server(settings, event_tx, command_rx));

        wait_for_connecting(&mut event_rx).await;
        let client = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let _client = client;

        timeout(
            RELIABLE_READ_TIMEOUT + Duration::from_secs(1),
            wait_for_disconnected(&mut event_rx),
        )
        .await
        .expect("agent should report disconnected after reliable idle timeout");

        command_tx.send(ConnectionCommand::Stop).unwrap();
        agent.await.unwrap().unwrap();
    }

    #[test]
    fn reliable_read_timeout_allows_drag_handoff_and_bulk_transfer_jitter() {
        assert!(RELIABLE_READ_TIMEOUT >= Duration::from_secs(5));
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
}
