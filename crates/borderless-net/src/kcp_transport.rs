use anyhow::{ensure, Context};
use borderless_core::protocol::{
    decode_frame, encode_frame, DecodedFrame, WireMessage, MAX_FRAME_LEN,
};
use kcp_tokio::{KcpConfig, KcpStream};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    sync::Mutex,
};

pub struct KcpListener {
    inner: Mutex<kcp_tokio::KcpListener>,
    local_addr: SocketAddr,
}

impl KcpListener {
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

pub struct KcpFramedTransport {
    stream: KcpStream,
    next_sequence: u64,
}

pub struct AcceptedKcpTransport {
    pub transport: KcpFramedTransport,
    pub peer: SocketAddr,
}

pub struct KcpFramedReader {
    reader: ReadHalf<KcpStream>,
}

pub struct KcpFramedWriter {
    writer: WriteHalf<KcpStream>,
    next_sequence: u64,
}

impl KcpFramedTransport {
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let addr = addr.parse::<SocketAddr>()?;
        let config = kcp_config();
        let stream = KcpStream::connect(addr, config).await?;
        Ok(Self {
            stream,
            next_sequence: 1,
        })
    }

    pub async fn accept(listener: &KcpListener) -> anyhow::Result<AcceptedKcpTransport> {
        let mut listener = listener.inner.lock().await;
        let (stream, peer) = listener.accept().await?;
        Ok(AcceptedKcpTransport {
            transport: Self {
                stream,
                next_sequence: 1,
            },
            peer,
        })
    }

    pub async fn bind(addr: &str) -> anyhow::Result<KcpListener> {
        let addr = addr.parse::<SocketAddr>()?;
        let inner = kcp_tokio::KcpListener::bind(addr, kcp_config()).await?;
        let local_addr = *inner.local_addr();
        Ok(KcpListener {
            inner: Mutex::new(inner),
            local_addr,
        })
    }

    pub async fn send(&mut self, message: &WireMessage) -> anyhow::Result<()> {
        send_frame(&mut self.stream, &mut self.next_sequence, message).await
    }

    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        read_frame_from(&mut self.stream).await
    }

    pub fn split(self) -> (KcpFramedReader, KcpFramedWriter) {
        let (reader, writer) = tokio::io::split(self.stream);
        (
            KcpFramedReader { reader },
            KcpFramedWriter {
                writer,
                next_sequence: self.next_sequence,
            },
        )
    }
}

impl KcpFramedReader {
    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        read_frame_from(&mut self.reader).await
    }
}

impl KcpFramedWriter {
    pub async fn send(&mut self, message: &WireMessage) -> anyhow::Result<()> {
        send_frame(&mut self.writer, &mut self.next_sequence, message).await
    }
}

async fn send_frame<W>(
    writer: &mut W,
    next_sequence: &mut u64,
    message: &WireMessage,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let frame = encode_frame(*next_sequence, message)?;
    ensure!(
        frame.len() <= MAX_FRAME_LEN,
        "encoded frame too large: max {}, got {}",
        MAX_FRAME_LEN,
        frame.len()
    );
    let len = u32::try_from(frame.len()).context("frame length does not fit in u32")?;

    *next_sequence += 1;
    writer.write_u32(len).await?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame_from<R>(reader: &mut R) -> anyhow::Result<DecodedFrame>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await? as usize;
    ensure!(
        len <= MAX_FRAME_LEN,
        "incoming frame too large: max {}, got {}",
        MAX_FRAME_LEN,
        len
    );

    let mut raw = vec![0; len];
    reader.read_exact(&mut raw).await?;
    Ok(decode_frame(&raw)?)
}

fn kcp_config() -> KcpConfig {
    KcpConfig::new().fast_mode().stream_mode(true)
}

#[cfg(test)]
mod tests {
    use borderless_core::{
        geometry::Rect,
        protocol::{Hello, WireMessage, PROTOCOL_VERSION},
    };

    #[tokio::test]
    async fn kcp_transport_sends_and_receives_hello() {
        let listener = crate::kcp_transport::KcpFramedTransport::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let accepted = crate::kcp_transport::KcpFramedTransport::accept(&listener)
                .await
                .unwrap();
            let mut transport = accepted.transport;
            transport.read_frame().await.unwrap().message
        });

        let mut client = crate::kcp_transport::KcpFramedTransport::connect(&addr.to_string())
            .await
            .unwrap();
        client
            .send(&WireMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                desktop: Rect::new(0, 0, 1920, 1080),
            }))
            .await
            .unwrap();

        assert!(matches!(server.await.unwrap(), WireMessage::Hello(_)));
    }

    #[tokio::test]
    async fn kcp_accept_returns_peer_address() {
        let listener = crate::kcp_transport::KcpFramedTransport::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            crate::kcp_transport::KcpFramedTransport::accept(&listener)
                .await
                .unwrap()
                .peer
        });

        let _client = crate::kcp_transport::KcpFramedTransport::connect(&addr.to_string())
            .await
            .unwrap();
        let peer = server.await.unwrap();

        assert_eq!(peer.ip(), addr.ip());
    }
}
