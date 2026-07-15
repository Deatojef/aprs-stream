//! Decoder/demodulator tuning.
//!
//! Vendored from `aprs-rtp::config`, keeping only the source-agnostic
//! `DecoderConfig` (+ `FixBits`). The RTP-specific `SourceConfig` was dropped —
//! audio sourcing is no longer this crate's concern; it consumes `AudioBlock`s.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecoderConfig {
    /// Mark tone frequency in Hz. Default: 1200.
    #[serde(default = "default_mark_hz")]
    pub mark_hz: u32,
    /// Space tone frequency in Hz. Default: 2200.
    #[serde(default = "default_space_hz")]
    pub space_hz: u32,
    /// Baud rate. Default: 1200.
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// Number of parallel amplitude-imbalance slicers (1–16). Default: 8.
    #[serde(default = "default_slicers")]
    pub slicers: usize,
    /// Lowest rung of the slicer ladder, in **twist dB** (mark-minus-space).
    /// Default: -12.0.
    ///
    /// The slicer bank spreads `slicers` rungs evenly across
    /// `[min_twist_db, max_twist_db]` (see `afsk::slicer::space_gains`); uniform
    /// spacing in dB is a geometric progression in linear gain. Each rung
    /// compensates a different mark/space amplitude imbalance: negative dB is
    /// tuned for space louder than mark, 0 dB for a balanced signal, positive dB
    /// for mark louder. Narrowing the range concentrates resolution where a given
    /// station's imbalance actually lands; widening it covers more imbalance but
    /// spends slicers on twists that may never decode anything. Tune to your
    /// receiver / location.
    #[serde(default = "default_min_twist_db")]
    pub min_twist_db: f32,
    /// Highest rung of the slicer ladder, in twist dB. Default: 9.0. See
    /// `min_twist_db`. The default -12..+9 range over 8 slicers is a 3 dB step
    /// with a rung landing exactly on 0 dB.
    #[serde(default = "default_max_twist_db")]
    pub max_twist_db: f32,
    /// CRC error-recovery mode.
    #[serde(default)]
    pub fix_bits: FixBits,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            mark_hz: default_mark_hz(),
            space_hz: default_space_hz(),
            baud: default_baud(),
            slicers: default_slicers(),
            min_twist_db: default_min_twist_db(),
            max_twist_db: default_max_twist_db(),
            fix_bits: FixBits::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FixBits {
    None,
    #[default]
    Single,
    Double,
}

fn default_mark_hz() -> u32 {
    1200
}
fn default_space_hz() -> u32 {
    2200
}
fn default_baud() -> u32 {
    1200
}
fn default_slicers() -> usize {
    8
}
fn default_min_twist_db() -> f32 {
    -12.0
}
fn default_max_twist_db() -> f32 {
    9.0
}
