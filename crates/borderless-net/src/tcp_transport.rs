use anyhow::{ensure, Context};
use borderless_core::protocol::{
    decode_frame, encode_frame, DecodedFrame, WireMessage, MAX_FRAME_LEN,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
};

pub struct TcpFramedTransport {
    stream: TcpStream,
    next_sequence: u64,
}

pub struct TcpFramedReader {
    reader: ReadHalf<TcpStream>,
}

pub struct TcpFramedWriter {
    writer: WriteHalf<TcpStream>,
    next_sequence: u64,
}

impl TcpFramedTransport {
    pub fn new(stream: TcpStream) -> anyhow::Result<Self> {
        stream.set_nodelay(true).context("enable TCP_NODELAY")?;
        Ok(Self {
            stream,
            next_sequence: 1,
        })
    }

    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        Self::new(TcpStream::connect(addr).await?)
    }

    pub async fn send(&mut self, message: &WireMessage) -> anyhow::Result<()> {
        send_frame(&mut self.stream, &mut self.next_sequence, message).await
    }

    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        read_frame_from(&mut self.stream).await
    }

    pub fn split(self) -> (TcpFramedReader, TcpFramedWriter) {
        let (reader, writer) = tokio::io::split(self.stream);
        (
            TcpFramedReader { reader },
            TcpFramedWriter {
                writer,
                next_sequence: self.next_sequence,
            },
        )
    }
}

impl TcpFramedReader {
    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        read_frame_from(&mut self.reader).await
    }
}

impl TcpFramedWriter {
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

#[cfg(test)]
mod tests {
    use borderless_core::{
        geometry::Rect,
        protocol::{Hello, WireMessage, MAX_FRAME_LEN, PROTOCOL_VERSION},
    };
    use tokio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
    };

    #[tokio::test]
    async fn tcp_transport_sends_and_receives_hello() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut transport = crate::tcp_transport::TcpFramedTransport::new(stream).unwrap();
            transport.read_frame().await.unwrap().message
        });

        let mut client = crate::tcp_transport::TcpFramedTransport::connect(&addr.to_string())
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
    async fn tcp_transport_rejects_oversized_advertised_frame_before_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_u32(u32::try_from(MAX_FRAME_LEN + 1).unwrap())
                .await
                .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut transport = crate::tcp_transport::TcpFramedTransport::new(stream).unwrap();
        let err = transport.read_frame().await.unwrap_err();

        assert!(err.to_string().contains("incoming frame too large"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_peer_causes_read_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client = crate::tcp_transport::TcpFramedTransport::new(stream).unwrap();
        server.await.unwrap();
        assert!(client.read_frame().await.is_err());
    }
}
