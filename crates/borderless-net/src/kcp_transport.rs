use anyhow::{ensure, Context};
use borderless_core::protocol::{
    decode_frame, encode_frame, DecodedFrame, WireMessage, MAX_FRAME_LEN,
};
use kcp_tokio::{KcpConfig, KcpStream};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
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

    pub async fn accept(listener: &KcpListener) -> anyhow::Result<Self> {
        let mut listener = listener.inner.lock().await;
        let (stream, _) = listener.accept().await?;
        Ok(Self {
            stream,
            next_sequence: 1,
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
            let mut transport = crate::kcp_transport::KcpFramedTransport::accept(&listener)
                .await
                .unwrap();
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
}
