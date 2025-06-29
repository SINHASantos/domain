// Using tokio::io::duplex() seems appealing but it can only create a channel
// between two ends, it isn't possible to create additional client ends for a
// single server end for example.
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use core::time::Duration;

use std::boxed::Box;
use std::collections::HashMap;
use std::future::ready;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::vec::Vec;
use std::{cmp, io};

use futures_util::FutureExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tracing::trace;

use crate::net::client::protocol::{
    AsyncConnect, AsyncDgramRecv, AsyncDgramSend,
};
use crate::net::server::sock::{AsyncAccept, AsyncDgramSock};

// If MSRV gets bumped to 1.69.0 we can replace these with a const SocketAddr.
pub const DEF_CLIENT_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const DEF_CLIENT_PORT: u16 = 0;

enum Data {
    DgramRequest(SocketAddr, Vec<u8>),
    StreamAccept(ClientServerChannel),
    StreamRequest(Vec<u8>),
}

#[derive(Default)]
struct ReadBufBuffer {
    /// Buffer for received bytes that overflowed the client read buffer.
    buf: Vec<u8>,

    /// The index of the first unconsumed byte in last_read_buf.
    start_idx: usize,
}

impl ReadBufBuffer {
    pub fn extend(&mut self, data: Vec<u8>) -> &mut Self {
        self.buf.extend(data);
        self
    }

    pub fn fill(&mut self, target: &mut ReadBuf<'_>) -> usize {
        let num_unread_bytes = self.buf.len() - self.start_idx;
        let waiting_bytes_to_take =
            cmp::min(target.remaining(), num_unread_bytes);
        if waiting_bytes_to_take > 0 {
            let start = self.start_idx;
            let end = start + waiting_bytes_to_take;
            target.put_slice(&self.buf[start..end]);
            if end >= self.buf.len() {
                self.buf.clear();
                self.start_idx = 0;
            } else {
                self.start_idx = end;
            }
        }
        waiting_bytes_to_take
    }
}

struct ClientSocket {
    /// Sender for sender requests to the server.
    tx: mpsc::Sender<Data>,

    /// Receiver for receiving responses from the server.
    ///
    /// Wrapped in a mutex so that it can be mutated even from a &self fn.
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,

    /// Buffer for received bytes that overflowed the client read buffer.
    unread_buf: ReadBufBuffer,
}

impl ClientSocket {
    /// Create a connected client socket.
    ///
    /// Creates a new connection to the server.
    ///
    /// Receives `client_tx` which it should use to send messages to the
    /// server.
    ///
    /// Also creates its own sender/receive pair of which the sender half is
    /// returned so that the server can use it to send responses to the
    /// client, and the receiver half is kept for receiving the responses sent
    /// by the server.
    fn new(client_tx: mpsc::Sender<Data>) -> (Self, mpsc::Sender<Vec<u8>>) {
        let (server_tx, client_rx) = mpsc::channel(10);

        let res = Self {
            tx: client_tx,
            rx: Mutex::new(client_rx),
            unread_buf: Default::default(),
        };

        (res, server_tx)
    }
}

/// The server half of a client <-> server bidirectional connection.
///
/// Messages sent by clients must be attributed to the senders address
/// otherwise the server has no way of knowing from which client the request
/// came and thus to which client to respond to.
struct ServerSocket {
    /// Receiver for the server to receive messages from clients.
    ///
    /// Only the server uses this receiver.
    rx: mpsc::Receiver<Data>,

    /// Sender for sending messages by clients to the server.
    ///
    /// Each client receives a clone of this sender.
    tx: mpsc::Sender<Data>,

    /// Senders for the server to send responses to clients.
    ///
    /// One per client to which responses must be sent.
    response_txs: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>,

    /// Buffer for received bytes that overflowed the server read buffer.
    unread_buf: ReadBufBuffer,
}

impl Default for ServerSocket {
    fn default() -> Self {
        let (server_tx, server_rx) = mpsc::channel(10);

        // The server needs a fixed receiver where it receives messages from
        // all clients, and a list of senders one per client it has to send a
        // response to.
        Self {
            rx: server_rx,
            tx: server_tx,
            response_txs: Default::default(),
            unread_buf: Default::default(),
        }
    }
}

impl ServerSocket {
    fn sender(&self) -> mpsc::Sender<Data> {
        self.tx.clone()
    }
}

pub struct ClientServerChannel {
    /// Details of the server end of the connection.
    server: Arc<Mutex<ServerSocket>>,

    /// Details of the client end of the connection, if connected.
    client: Option<ClientSocket>,

    /// Simulated client address.
    client_addr: SocketAddr,

    /// Next mock client port number to use.
    next_client_port: Arc<AtomicU16>,

    /// Type of connection.
    is_stream: bool,

    /// Is connection terminated?
    is_shutdown: Arc<AtomicBool>,
}

impl Default for ClientServerChannel {
    fn default() -> Self {
        let client_addr = SocketAddr::new("127.0.0.1".parse().unwrap(), 0);

        Self {
            server: Default::default(),
            client: Default::default(),
            client_addr,
            next_client_port: Arc::new(AtomicU16::new(1)),
            is_stream: Default::default(),
            is_shutdown: Default::default(),
        }
    }
}

impl Clone for ClientServerChannel {
    /// Clones only the server half, the client half cannot be cloned. The
    /// result can be used to connect a new client to an existing server.
    fn clone(&self) -> Self {
        Self {
            server: self.server.clone(),
            client: None,
            client_addr: self.client_addr,
            next_client_port: self.next_client_port.clone(),
            is_stream: self.is_stream,
            is_shutdown: self.is_shutdown.clone(),
        }
    }
}

impl ClientServerChannel {
    pub fn new_dgram() -> Self {
        Self {
            is_stream: false,
            ..Default::default()
        }
    }

    pub fn new_stream() -> Self {
        Self {
            is_stream: true,
            ..Default::default()
        }
    }

    pub fn new_client(&self, client_addr: Option<SocketAddr>) -> Self {
        let mut client_addr = client_addr.unwrap_or_else(|| {
            SocketAddr::new("127.0.0.1".parse().unwrap(), 0)
        });

        if client_addr.port() == 0 {
            let client_port =
                self.next_client_port.fetch_add(1, Ordering::SeqCst);
            client_addr.set_port(client_port);
        }

        Self {
            server: self.server.clone(),
            client: None,
            client_addr,
            next_client_port: self.next_client_port.clone(),
            is_stream: self.is_stream,
            is_shutdown: self.is_shutdown.clone(),
        }
    }

    pub fn connect(&self, client_addr: Option<SocketAddr>) -> Self {
        fn setup_client(
            server_socket: &mut ServerSocket,
            client_addr: SocketAddr,
        ) -> ClientSocket {
            // Create a client socket for sending requests to the server.
            let (client, response_tx) =
                ClientSocket::new(server_socket.sender());

            // Tell the server how to respond to the client.
            server_socket.response_txs.insert(client_addr, response_tx);

            // Return the created client socket
            client
        }

        let client_addr = client_addr.unwrap_or_else(|| {
            let client_port =
                self.next_client_port.fetch_add(1, Ordering::SeqCst);
            SocketAddr::new("127.0.0.1".parse().unwrap(), client_port)
        });

        match self.is_stream {
            false => {
                // For dgram connections all clients communicate with the same
                // single server socket.
                let server_socket = &mut self.server.lock().unwrap();
                let client = setup_client(server_socket, client_addr);

                // Tell the client how to contact the server.
                Self {
                    server: self.server.clone(),
                    client: Some(client),
                    client_addr: self.client_addr,
                    next_client_port: self.next_client_port.clone(),
                    is_stream: false,
                    is_shutdown: Arc::new(AtomicBool::new(false)),
                }
            }

            true => {
                // But for stream connections each new client communicates
                // with a new server-side connection handler socket.
                let mut server_socket = ServerSocket::default();
                let client = setup_client(&mut server_socket, client_addr);

                // Tell the client how to contact the new server connection handler.
                let channel = Self {
                    server: Arc::new(Mutex::new(server_socket)),
                    client: Some(client),
                    client_addr,
                    next_client_port: self.next_client_port.clone(),
                    is_stream: true,
                    is_shutdown: Arc::new(AtomicBool::new(false)),
                };

                // Tell the server how to receive from and respond to the client
                // by unblocking AsyncAccept::poll_accept() which is being polled
                // by the server.
                let sender = self.server.lock().unwrap().tx.clone();
                let channel_for_client =
                    channel.new_client(Some(client_addr));
                tokio::spawn(async move {
                    sender.send(Data::StreamAccept(channel_for_client)).await
                });

                channel
            }
        }
    }
}

//--- AsynConnect
//
// Dgram connection establishment

impl AsyncConnect for ClientServerChannel {
    type Connection = ClientServerChannel;

    type Fut = Pin<
        Box<
            dyn Future<Output = Result<Self::Connection, io::Error>>
                + Send
                + Sync,
        >,
    >;

    fn connect(&self) -> Self::Fut {
        let conn = self.connect(Some(self.client_addr));
        Box::pin(async move { Ok(conn) })
    }
}

//--- AsyncDgramRecv
//
// Dgram client socket.

impl AsyncDgramRecv for ClientServerChannel {
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let mut rx = self.client.as_ref().unwrap().rx.lock().unwrap();
        match rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                trace!(
                    "Reading {} bytes from dgram client channel",
                    data.len()
                );
                buf.put_slice(&data);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => {
                trace!("Broken pipe while reading in dgram client channel (is_closed={})", rx.is_closed());
                Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
            }
            Poll::Pending => {
                trace!("Pending read in dgram client channel");
                Poll::Pending
            }
        }
    }
}

impl AsyncDgramSend for ClientServerChannel {
    fn poll_send(
        &self,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match &self.client {
            Some(client) => {
                let msg = Data::DgramRequest(self.client_addr, data.into());

                // TODO: Can Stelline scripts mix and match fake responses with
                // responses from a real server? Do we need to first try
                // invoking do_server() to see if the .rpl script defined a
                // hard-coded answer for this query (like net::stelline::dgram
                // does), before attempting to dispatch it to the configured
                // server?

                let mut fut = Box::pin(client.tx.send(msg));
                match fut.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        trace!(
                            "Sent {} bytes in dgram client channel",
                            data.len(),
                        );
                        Poll::Ready(Ok(data.len()))
                    }
                    Poll::Ready(Err(_)) => {
                        trace!(
                            "Broken pipe while sending in dgram client channel");
                        Poll::Ready(Err(io::Error::from(
                            io::ErrorKind::BrokenPipe,
                        )))
                    }
                    Poll::Pending => {
                        trace!("Pending write in dgram client channel");
                        Poll::Pending
                    }
                }
            }

            None => {
                trace!("Unable to send bytes in dgram client channel: not connected");
                Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected)))
            }
        }
    }
}

//--- AsyncDgramSock
//
// Dgram server socket.

impl AsyncDgramSock for ClientServerChannel {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        data: &[u8],
        dest: &SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let server_socket = self.server.lock().unwrap();
        let tx = server_socket.response_txs.get(dest);
        if let Some(server_tx) = tx {
            let mut fut = Box::pin(server_tx.send(data.to_vec()));
            match fut.poll_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    trace!(
                        "Sent {} bytes to {} in dgram server channel",
                        data.len(),
                        dest
                    );
                    Poll::Ready(Ok(data.len()))
                }
                Poll::Ready(Err(_)) => {
                    trace!(
                        "Broken pipe while writing in dgram server channel"
                    );
                    Poll::Ready(Err(io::Error::from(
                        io::ErrorKind::BrokenPipe,
                    )))
                }
                Poll::Pending => {
                    trace!("Pending write in dgram server channel");
                    Poll::Pending
                }
            }
        } else {
            trace!(
                "Unable to send bytes in dgram server channel: not connected"
            );
            Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected)))
        }
    }

    fn readable(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_ + Send>> {
        Box::pin(ClientServerChannelReadableFut(self.server.clone()))
    }

    fn try_recv_buf_from(
        &self,
        buf: &mut ReadBuf<'_>,
    ) -> io::Result<(usize, SocketAddr)> {
        let mut server_socket = self.server.lock().unwrap();
        let rx = &mut server_socket.rx;
        match rx.try_recv() {
            Ok(Data::DgramRequest(addr, data)) => {
                // TODO: use unread buf here to prevent overflow of given buf.
                trace!("Reading {} bytes from {addr} into buffer of len {} in dgram server channel", data.len(), buf.remaining());
                buf.put_slice(&data);
                Ok((data.len(), addr))
            }
            Ok(Data::StreamAccept(..)) => unreachable!(),
            Ok(Data::StreamRequest(..)) => unreachable!(),
            Err(TryRecvError::Disconnected) => {
                trace!("Broken pipe while reading in dgram server channel");
                Err(io::ErrorKind::BrokenPipe.into())
            }
            Err(TryRecvError::Empty) => {
                trace!("Pending read in dgram server channel");
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
    }
}

pub struct ClientServerChannelReadableFut(Arc<Mutex<ServerSocket>>);

impl Future for ClientServerChannelReadableFut {
    type Output = io::Result<()>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        let server_socket = self.0.lock().unwrap();
        let rx = &server_socket.rx;
        if !rx.is_empty() {
            trace!("Server socket is now readable");
            Poll::Ready(Ok(()))
        } else {
            let waker = cx.waker().clone();
            tokio::task::spawn(async move {
                // Give other tasks a chance to run.
                tokio::time::sleep(Duration::from_nanos(1)).await;
                waker.wake();
            });
            Poll::Pending
        }
    }
}

//--- AsyncAccept
//
// Stream connection establishment

impl AsyncAccept for ClientServerChannel {
    type Error = io::Error;
    type StreamType = ClientServerChannel;
    type Future = std::future::Ready<Result<Self::StreamType, io::Error>>;

    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Self::Future, SocketAddr)>> {
        let mut server_socket = self.server.lock().unwrap();
        let rx = &mut server_socket.rx;
        match rx.poll_recv(cx) {
            // This will become ready when ClientServerChannel::connect()
            // sends the details of a new client connection to us.
            Poll::Ready(Some(Data::StreamAccept(channel))) => {
                trace!("Accepted connection in stream channel",);
                let client_addr = channel.client_addr;
                Poll::Ready(Ok((ready(Ok(channel)), client_addr)))
            }
            Poll::Ready(Some(Data::StreamRequest(..))) => unreachable!(),
            Poll::Ready(Some(Data::DgramRequest(..))) => unreachable!(),
            Poll::Ready(None) => {
                trace!("Broken pipe while accepting in stream channel");
                Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
            }
            Poll::Pending => {
                trace!("Pending accept in stream channel");
                Poll::Pending
            }
        }
    }
}

//--- AsyncRead
//
// Stream server socket read (from client)
// Client socket read (from server)

impl AsyncRead for ClientServerChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_shutdown.load(Ordering::SeqCst) {
            trace!(
                "Stream shutdown while reading in client-server stream channel"
            );
            return Poll::Ready(Err(io::Error::from(
                io::ErrorKind::BrokenPipe,
            )));
        }

        match &mut self.client {
            Some(client) => {
                let rx = &mut client.rx.lock().unwrap();
                match rx.poll_recv(cx) {
                    Poll::Ready(Some(data)) => {
                        trace!("Reading {} bytes into internal buffer in client stream channel", data.len());
                        client.unread_buf.extend(data).fill(buf);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(None) => {
                        trace!("Broken pipe while reading in client stream channel");
                        Poll::Ready(Err(io::Error::from(
                            io::ErrorKind::BrokenPipe,
                        )))
                    }
                    Poll::Pending => {
                        trace!("Pending read in client stream channel");
                        if client.unread_buf.fill(buf) > 0 {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }
            None => {
                let mut server_socket = self.server.lock().unwrap();
                let rx = &mut server_socket.rx;
                match rx.poll_recv(cx) {
                    Poll::Ready(Some(Data::StreamRequest(data))) => {
                        trace!("Reading {} bytes into internal buffer in server stream channel", data.len());
                        server_socket.unread_buf.extend(data).fill(buf);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Some(Data::StreamAccept(..))) => {
                        unreachable!()
                    }
                    Poll::Ready(Some(Data::DgramRequest(..))) => {
                        unreachable!()
                    }
                    Poll::Ready(None) => {
                        trace!("Broken pipe while reading in server stream channel");

                        Poll::Ready(Err(io::Error::from(
                            io::ErrorKind::BrokenPipe,
                        )))
                    }
                    Poll::Pending => {
                        trace!("Pending read in server stream channel");
                        if server_socket.unread_buf.fill(buf) > 0 {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }
        }
    }
}

//--- AsyncWrite
//
// Stream server socket write (to client)
// Client socket write (to server)

impl AsyncWrite for ClientServerChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.is_shutdown.load(Ordering::SeqCst) {
            trace!(
                "Stream shutdown while writing in client-server stream channel"
            );
            return Poll::Ready(Err(io::Error::from(
                io::ErrorKind::BrokenPipe,
            )));
        }

        match &self.client {
            Some(client) => {
                let mut fut = Box::pin(
                    client.tx.send(Data::StreamRequest(data.to_vec())),
                );
                match fut.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        trace!(
                            "Sent {} bytes in client stream channel",
                            data.len(),
                        );
                        Poll::Ready(Ok(data.len()))
                    }
                    Poll::Ready(Err(_)) => {
                        trace!("Broken pipe while writing in client stream channel");
                        Poll::Ready(Err(io::Error::from(
                            io::ErrorKind::BrokenPipe,
                        )))
                    }
                    Poll::Pending => {
                        trace!("Pending write in client stream channel");
                        Poll::Pending
                    }
                }
            }
            None => {
                let server_socket = self.server.lock().unwrap();
                let tx = server_socket.response_txs.iter().next();
                if let Some((_addr, server_tx)) = tx {
                    let mut fut = Box::pin(server_tx.send(data.to_vec()));
                    match fut.poll_unpin(cx) {
                        Poll::Ready(Ok(())) => {
                            trace!(
                                "Sent {} bytes in server stream channel",
                                data.len(),
                            );
                            Poll::Ready(Ok(data.len()))
                        }
                        Poll::Ready(Err(_)) => {
                            trace!("Broken pipe while writing in server stream channel");
                            Poll::Ready(Err(io::Error::from(
                                io::ErrorKind::BrokenPipe,
                            )))
                        }
                        Poll::Pending => {
                            trace!("Pending write in server stream channel");
                            Poll::Pending
                        }
                    }
                } else {
                    trace!("Failed write in server stream channel: not connected");
                    Poll::Ready(Err(io::Error::from(
                        io::ErrorKind::NotConnected,
                    )))
                }
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        // NO OP
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        self.is_shutdown.store(true, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}
