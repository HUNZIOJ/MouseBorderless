use anyhow::{ensure, Context};
use borderless_core::protocol::{
    decode_frame, encode_frame, DecodedFrame, WireMessage, MAX_FRAME_LEN,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub struct TcpFramedTransport {
    stream: TcpStream,
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
        let frame = encode_frame(self.next_sequence, message)?;
        ensure!(
            frame.len() <= MAX_FRAME_LEN,
            "encoded frame too large: max {}, got {}",
            MAX_FRAME_LEN,
            frame.len()
        );
        let len = u32::try_from(frame.len()).context("frame length does not fit in u32")?;

        self.next_sequence += 1;
        self.stream.write_u32(len).await?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        let len = self.stream.read_u32().await? as usize;
        ensure!(
            len <= MAX_FRAME_LEN,
            "incoming frame too large: max {}, got {}",
            MAX_FRAME_LEN,
            len
        );

        let mut raw = vec![0; len];
        self.stream.read_exact(&mut raw).await?;
        Ok(decode_frame(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use borderless_core::{
        geometry::Rect,
        protocol::{Hello, WireMessage, PROTOCOL_VERSION},
    };
    use tokio::net::TcpListener;

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
}
