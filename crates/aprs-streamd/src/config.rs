//! TOML configuration for aprs-streamd.
//!
//! The `[source]` and `[decoder]` tables deserialize directly into aprs-rtp's own
//! config types (single source of truth — no re-modeling), and `[emit]` maps onto
//! `aprs-stream`'s emitter settings.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Candidate config locations, in priority order, after any explicit CLI arg or
/// `$APRS_STREAMD_CONFIG`. The second entry is the intended deployed location.
const SEARCH_PATHS: &[&str] = &["config.toml", "/etc/aprs-streamd/config.toml"];

/// Top-level configuration file.
///
/// Exactly one audio source must be configured: `[source]` (ka9q-radio RTP) or
/// `[sdr]` (direct RTL-SDR). Both share the `[decoder]` tuning and `[emit]` side,
/// so flipping between them for a like-for-like comparison is a one-section edit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RTP audio source (ka9q-radio). Mutually exclusive with `[sdr]`.
    #[serde(default)]
    pub source: Option<aprs_rtp::config::SourceConfig>,
    /// Direct RTL-SDR source. Mutually exclusive with `[source]`.
    #[serde(default)]
    pub sdr: Option<SdrSection>,
    /// Decoder/demodulator tuning. Optional; defaults to aprs-rtp's defaults.
    #[serde(default)]
    pub decoder: aprs_rtp::config::DecoderConfig,
    /// Publish side. Optional; defaults to the on-subnet multicast group.
    #[serde(default)]
    pub emit: EmitSection,
}

/// The `[sdr]` table — direct RTL-SDR capture (see `aprs-sdr`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdrSection {
    /// Dongle index.
    #[serde(default)]
    pub device: usize,
    /// Tuner centre frequency, Hz. Offset from the channels so none sits on DC.
    pub center_hz: u32,
    /// Channel centre frequencies, Hz. Each becomes `ssrc = freq_kHz`.
    pub channels_hz: Vec<u32>,
    /// Complex sample rate, Hz.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    /// Tuner gain: a number (tenths of a dB, e.g. 400), `"auto"` (software gain
    /// manager holding the noise floor at `auto_floor_dbfs`), or `"hw-agc"` (the
    /// tuner's own AGC — rarely wanted; overload-prone).
    #[serde(default = "default_gain")]
    pub gain: GainSetting,
    /// Noise-floor setpoint in dBFS for `gain = "auto"`. Higher = more gain (better
    /// sensitivity, less headroom). See the `front-end level` log line.
    #[serde(default = "default_auto_floor")]
    pub auto_floor_dbfs: f32,
    /// Frequency correction, ppm.
    #[serde(default)]
    pub ppm: i32,
    /// FM deviation (Hz) mapped to full-scale audio — the `rec` level knob.
    #[serde(default)]
    pub fm_maxdev_hz: Option<f32>,
    /// Squelch open threshold, dB SNR.
    #[serde(default)]
    pub squelch_open_db: Option<f32>,
    /// Squelch close threshold, dB SNR.
    #[serde(default)]
    pub squelch_close_db: Option<f32>,
    /// Enable ka9q-style FM de-emphasis (default off).
    #[serde(default)]
    pub deemphasis: bool,
}

/// Tuner gain from TOML: `gain = 400` (tenths dB) or `gain = "auto"` (AGC).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GainSetting {
    Tenths(i32),
    Mode(String),
}

fn default_sample_rate() -> u32 {
    1_200_000
}
fn default_gain() -> GainSetting {
    GainSetting::Tenths(400)
}
fn default_auto_floor() -> f32 {
    aprs_sdr::source::DEFAULT_AUTO_FLOOR_DBFS
}

/// The `[emit]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitSection {
    /// Multicast group to publish on.
    #[serde(default = "default_group")]
    pub group: Ipv4Addr,
    /// UDP port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Local interface to send from (`0.0.0.0` = OS chooses).
    #[serde(default = "default_interface")]
    pub interface: Ipv4Addr,
    /// Multicast TTL (1 = stay on-subnet).
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    /// Extra unicast destinations (cross-VLAN escape hatch).
    #[serde(default)]
    pub destinations: Vec<SocketAddr>,
}

impl EmitSection {
    /// All destinations for each datagram: the multicast group first, then any
    /// configured unicast targets.
    pub fn destinations(&self) -> Vec<SocketAddr> {
        let mut all = Vec::with_capacity(1 + self.destinations.len());
        all.push(SocketAddr::new(self.group.into(), self.port));
        all.extend(self.destinations.iter().copied());
        all
    }
}

impl Default for EmitSection {
    fn default() -> Self {
        Self {
            group: default_group(),
            port: default_port(),
            interface: default_interface(),
            ttl: default_ttl(),
            destinations: Vec::new(),
        }
    }
}

impl Config {
    /// Resolve the config path, read it, and parse it. Returns the path used so
    /// the caller can log which file won.
    pub fn load() -> Result<(PathBuf, Self), ConfigError> {
        let path = resolve_path().ok_or_else(|| ConfigError::NotFound {
            searched: SEARCH_PATHS.join(", "),
        })?;
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let cfg = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        Ok((path, cfg))
    }
}

/// Find the config file: explicit CLI arg, then `$APRS_STREAMD_CONFIG`, then the
/// first existing entry in [`SEARCH_PATHS`].
fn resolve_path() -> Option<PathBuf> {
    if let Some(arg) = std::env::args().nth(1) {
        return Some(PathBuf::from(arg));
    }
    if let Ok(env) = std::env::var("APRS_STREAMD_CONFIG") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    SEARCH_PATHS
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(PathBuf::from)
}

fn default_group() -> Ipv4Addr {
    Ipv4Addr::new(239, 12, 34, 56)
}
fn default_interface() -> Ipv4Addr {
    Ipv4Addr::UNSPECIFIED
}
fn default_port() -> u16 {
    17_014
}
fn default_ttl() -> u32 {
    1
}

/// Errors from locating, reading, or parsing the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file found (searched: {searched}); pass a path as the first argument or set APRS_STREAMD_CONFIG")]
    NotFound { searched: String },
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}
