use thiserror::Error;

/// Errors surfaced by the modem. The RTP-transport variants from `aprs-rtp`'s
/// error type were dropped along with the RTP front-end; what remains covers the
/// source-agnostic decode path.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("channel closed")]
    ChannelClosed,
}

pub type Result<T> = std::result::Result<T, Error>;
