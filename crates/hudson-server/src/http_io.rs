//! Socket-level deadlines also cover HTTP body backpressure and upgrades.
//! A stream timer alone cannot close a socket that has stopped polling its body.
use axum::serve::Listener;
use std::{
    future::Future,
    io::{self, IoSlice},
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::{Instant, Sleep},
};

pub struct DeadlineListener(pub TcpListener);
impl Listener for DeadlineListener {
    type Io = DeadlineSocket;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (socket, address) = Listener::accept(&mut self.0).await;
        (
            DeadlineSocket::new(socket, Duration::from_secs(300), Duration::from_secs(5)),
            address,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
struct Idle {
    timer: Pin<Box<Sleep>>,
    duration: Duration,
    active: bool,
}
impl Idle {
    fn new(duration: Duration) -> Self {
        Self {
            timer: Box::pin(tokio::time::sleep(duration)),
            duration,
            active: false,
        }
    }
    fn observe<T>(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        match result {
            Poll::Ready(result) => {
                self.active = false;
                Poll::Ready(result)
            }
            Poll::Pending => {
                if !self.active {
                    self.timer.as_mut().reset(Instant::now() + self.duration);
                    self.active = true;
                }
                if self.timer.as_mut().poll(cx).is_ready() {
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "HTTP socket idle deadline exceeded",
                    )))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}
pub struct DeadlineSocket {
    socket: TcpStream,
    lifetime: Pin<Box<Sleep>>,
    read: Idle,
    write: Idle,
}
impl DeadlineSocket {
    fn new(socket: TcpStream, lifetime: Duration, write: Duration) -> Self {
        Self {
            socket,
            lifetime: Box::pin(tokio::time::sleep(lifetime)),
            read: Idle::new(Duration::from_secs(60)),
            write: Idle::new(write),
        }
    }
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.lifetime.as_mut().poll(cx).is_ready()
    }
}
fn expired<T>() -> Poll<io::Result<T>> {
    Poll::Ready(Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "HTTP socket lifetime exceeded",
    )))
}
impl AsyncRead for DeadlineSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return expired();
        }
        let result = Pin::new(&mut this.socket).poll_read(cx, buffer);
        this.read.observe(cx, result)
    }
}
impl AsyncWrite for DeadlineSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return expired();
        }
        let result = Pin::new(&mut this.socket).poll_write(cx, bytes);
        this.write.observe(cx, result)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return expired();
        }
        let result = Pin::new(&mut this.socket).poll_write_vectored(cx, bytes);
        this.write.observe(cx, result)
    }
    fn is_write_vectored(&self) -> bool {
        self.socket.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return expired();
        }
        let result = Pin::new(&mut this.socket).poll_flush(cx);
        this.write.observe(cx, result)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return expired();
        }
        let result = Pin::new(&mut this.socket).poll_shutdown(cx);
        this.write.observe(cx, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn pair(lifetime: Duration, write: Duration) -> (DeadlineSocket, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (DeadlineSocket::new(server, lifetime, write), client)
    }
    #[tokio::test]
    async fn lifetime_closes_an_idle_socket_without_body_polling() {
        let (mut server, _client) = pair(Duration::from_millis(40), Duration::from_secs(5)).await;
        let result = tokio::time::timeout(Duration::from_secs(2), server.read(&mut [0; 1]))
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
    #[tokio::test]
    async fn a_peer_that_stops_reading_cannot_hold_a_blocked_write_indefinitely() {
        let (mut server, _client) = pair(Duration::from_secs(30), Duration::from_millis(50)).await;
        let bytes = vec![0; 1024 * 1024];
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            for _ in 0..256 {
                server.write_all(&bytes).await?;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
