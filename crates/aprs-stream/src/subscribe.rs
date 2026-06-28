//! Multicast subscriber: join a group and receive decoded frames.
//!
//! Uses `socket2` for the controls std `UdpSocket` doesn't expose: `SO_REUSEADDR`
//! (so multiple consumers on one host can share the port), explicit interface
//! selection for the multicast join, and `SO_RCVBUF` sizing (a consumer's only
//! real defense against bursts — there is no producer-side backpressure).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::codec::{self, CodecError};
use crate::proto::AprsFrame;

/// Configuration for a [`Subscriber`].
#[derive(Debug, Clone)]
pub struct SubscribeConfig {
    /// Multicast group to join.
    pub group: Ipv4Addr,
    /// Port to bind / receive on.
    pub port: u16,
    /// Local interface to join the group on. `0.0.0.0` lets the OS pick; set it
    /// explicitly on a multi-homed host.
    pub interface: Ipv4Addr,
    /// Optional `SO_RCVBUF` size in bytes. Larger absorbs bigger bursts before a
    /// slow consumer starts dropping datagrams.
    pub recv_buffer_bytes: Option<usize>,
}

impl SubscribeConfig {
    /// Config for a group/port, joining on any interface with default buffering.
    pub fn new(group: Ipv4Addr, port: u16) -> Self {
        Self {
            group,
            port,
            interface: Ipv4Addr::UNSPECIFIED,
            recv_buffer_bytes: None,
        }
    }
}

/// Joined multicast receiver that yields decoded frames.
pub struct Subscriber {
    socket: UdpSocket,
}

impl Subscriber {
    /// Bind, join the multicast group, and return a ready subscriber.
    pub fn new(cfg: SubscribeConfig) -> std::io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Share the port with other consumers on the same host.
        socket.set_reuse_address(true)?;
        if let Some(size) = cfg.recv_buffer_bytes {
            socket.set_recv_buffer_size(size)?;
        }

        // Bind to the wildcard address (portable across platforms for multicast
        // receive) on the chosen port, then join the group on the interface.
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, cfg.port);
        socket.bind(&SocketAddr::from(bind_addr).into())?;
        socket.join_multicast_v4(&cfg.group, &cfg.interface)?;
        socket.set_nonblocking(true)?;

        let std_socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(std_socket)?;

        Ok(Self { socket })
    }

    /// Await the next datagram and decode it into a typed frame.
    ///
    /// Returns the frame and the sender's address. A decode error means a
    /// malformed or version-incompatible datagram; the caller decides whether to
    /// skip and keep receiving.
    pub async fn recv_frame(&self) -> Result<(AprsFrame, SocketAddr), RecvError> {
        // One frame per datagram, comfortably under the safe UDP payload size;
        // a 2 KiB buffer leaves headroom over the ~1472-byte ceiling.
        let mut buf = vec![0u8; 2048];
        let (len, from) = self.socket.recv_from(&mut buf).await?;
        let frame = codec::decode(&buf[..len])?;
        Ok((frame, from))
    }

    /// Borrow the underlying socket (e.g. for additional configuration).
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }
}

/// Errors from receiving a frame.
#[derive(Debug, thiserror::Error)]
pub enum RecvError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
