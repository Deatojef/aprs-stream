//! Connectionless, stateless frame emitter.
//!
//! The emitter sends each encoded frame to a configurable *list* of destinations
//! (decision #7: multicast is the same-L2 default; unicast is the escape hatch).
//! It is connectionless and keeps no per-consumer state — a vanished consumer is
//! a non-event, and there is no producer-side backpressure. The optional stateful
//! TCP fan-out is a separate component, not this.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::codec::{self, CodecError};
use crate::proto::AprsFrame;

/// Configuration for an [`Emitter`].
#[derive(Debug, Clone)]
pub struct EmitConfig {
    /// Local interface address to send from. On a multi-homed host, set this to
    /// the interface facing the APRS LAN rather than letting the OS choose.
    /// Defaults to `0.0.0.0` (OS picks).
    pub interface: Ipv4Addr,

    /// Every destination to send each datagram to. Typically the multicast group
    /// plus any explicit unicast targets (cross-VLAN relays, etc.).
    pub destinations: Vec<SocketAddr>,

    /// Multicast TTL. Default 1 keeps traffic on-subnet.
    pub multicast_ttl: u32,
}

impl Default for EmitConfig {
    fn default() -> Self {
        Self {
            interface: Ipv4Addr::UNSPECIFIED,
            destinations: Vec::new(),
            multicast_ttl: 1,
        }
    }
}

/// Sends encoded frames to a fixed list of destinations.
pub struct Emitter {
    socket: UdpSocket,
    destinations: Vec<SocketAddr>,
}

impl Emitter {
    /// Build an emitter bound to `cfg.interface`, ready to send to
    /// `cfg.destinations`.
    pub fn new(cfg: EmitConfig) -> std::io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.bind(&SocketAddr::from(SocketAddrV4::new(cfg.interface, 0)).into())?;
        // Explicit multicast egress: TTL (stay on-subnet by default) and the
        // outbound interface for multi-homed hosts.
        socket.set_multicast_ttl_v4(cfg.multicast_ttl)?;
        socket.set_multicast_if_v4(&cfg.interface)?;
        socket.set_nonblocking(true)?;

        let std_socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(std_socket)?;

        Ok(Self {
            socket,
            destinations: cfg.destinations,
        })
    }

    /// Encode and send a frame to every configured destination.
    ///
    /// Returns the encoded byte length on success. Per-destination send errors
    /// are returned as the first failure; the producer treats lost datagrams as
    /// acceptable (APRS is best-effort RF already).
    pub async fn send_frame(&self, frame: &AprsFrame) -> Result<usize, EmitError> {
        let bytes = codec::encode(frame)?;
        self.send_bytes(&bytes).await?;
        Ok(bytes.len())
    }

    /// Send already-encoded bytes to every configured destination.
    pub async fn send_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
        for dest in &self.destinations {
            self.socket.send_to(bytes, dest).await?;
        }
        Ok(())
    }

    /// The destinations this emitter sends to.
    pub fn destinations(&self) -> &[SocketAddr] {
        &self.destinations
    }
}

/// Errors from emitting a frame.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
