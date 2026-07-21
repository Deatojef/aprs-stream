use crate::afsk::dpll::{DemodBit, Dpll};

/// Convert a twist value in dB to the equivalent linear space-gain multiplier.
///
/// A slicer with space-gain `g` balances (`m_amp - g·s_amp == 0`) when the
/// received mark/space amplitude ratio equals `g`, i.e. when the signal carries
/// `20·log₁₀(g)` dB of mark-minus-space twist. So twist in dB *is* the natural
/// unit for a rung; this is the bridge back to the linear gain the math needs.
///   0 dB → 1.0 (balanced),  −6 dB → 0.5 (space 6 dB louder),  +6 dB → ~2.0.
#[inline]
pub fn twist_db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Precomputed space-gain factors for the multi-slicer array, laid out as a
/// uniform ladder in **twist dB**.
///
///   step_db        = (max_db - min_db) / (n - 1)   [arithmetic in dB]
///   twist_db[i]    = min_db + i · step_db
///   space_gain[i]  = twist_db_to_gain(twist_db[i])
///
/// The ladder spans `[min_db, max_db]` in `n` evenly-spaced dB steps. Uniform
/// spacing in dB is identical to a geometric progression in linear gain — that is
/// the whole point of the dB framing: each rung compensates an equal increment of
/// transmitter pre-emphasis / receiver de-emphasis imbalance, and the rungs read
/// as round numbers (−12, −9, −6, …) in logs.
///
/// The range is configurable (`DecoderConfig::min_twist_db` / `max_twist_db`,
/// defaults −12 / +9 dB → an 8-rung ladder at a 3 dB step) so an operator can
/// concentrate slicer resolution where their station's imbalance actually lands —
/// empirically the productive band is narrow and station-specific. The default is
/// skewed toward negative twist (space louder than mark): ~50k locally-received RF
/// packets decoded overwhelmingly on the low-gain rungs, the signature of flat /
/// discriminator audio carrying transmit pre-emphasis. With the default 3 dB step
/// the ladder lands a rung exactly on 0 dB (unity gain, a perfectly balanced
/// signal), which a pure linear min/max ladder never guaranteed.
///
/// A single slicer (`n == 1`) uses unity gain — 0 dB — regardless of the range
/// (the range only has meaning across multiple rungs), matching direwolf's
/// single-slicer case.
///
/// NOTE vs. direwolf: direwolf builds its ladder from hardcoded *linear* endpoints
/// (demod_afsk.c, 0.5/4.0) as a geometric progression over a fixed MAX_SUBCHANS (9)
/// denominator, then uses the first `num_slicers` of them. Geometric-in-linear and
/// uniform-in-dB are the same curve; we just parameterize it in dB and rescale to
/// exactly `n` rungs. The defaults differ on purpose (see above).
pub fn space_gains(n: usize, min_db: f32, max_db: f32) -> Vec<f32> {
    assert!(n >= 1);
    assert!(
        max_db >= min_db,
        "max_twist_db ({max_db}) must be >= min_twist_db ({min_db})"
    );
    if n == 1 {
        return vec![1.0]; // single slicer: unity gain (0 dB)
    }
    let step_db = (max_db - min_db) / (n - 1) as f32;
    (0..n)
        .map(|i| twist_db_to_gain(min_db + i as f32 * step_db))
        .collect()
}

/// Per-slicer state: one DPLL per slicer, driven by a unique space_gain.
///
/// In the multi-slicer path, all slicers receive the same mark/space amplitude
/// measurements each sample but apply a different `space_gain` before comparing:
///   demod_out = m_amp - s_amp * space_gain[i]
///   amplitude = 0.5 * (m_peak - m_valley + (s_peak - s_valley) * space_gain[i])
///
/// This makes the decoder robust to amplitude imbalance between the tones
/// without needing to know a priori which direction the imbalance goes.
pub struct SlicerBank {
    pub dplls: Vec<Dpll>,
    pub gains: Vec<f32>,
}

impl SlicerBank {
    pub fn new(
        num_slicers: usize,
        min_twist_db: f32,
        max_twist_db: f32,
        baud: u32,
        sample_rate: u32,
    ) -> Self {
        let gains = space_gains(num_slicers, min_twist_db, max_twist_db);
        let dplls = (0..num_slicers)
            .map(|i| Dpll::new(baud, sample_rate, i))
            .collect();
        Self { dplls, gains }
    }

    /// Drive all slicers for one audio sample.
    ///
    /// `m_amp`, `s_amp`: mark and space envelope magnitudes from IQ detection.
    ///
    /// Returns bits from any slicers whose DPLLs overflowed this sample.
    pub fn process(&mut self, m_amp: f32, s_amp: f32) -> Vec<DemodBit> {
        let mut bits = Vec::new();
        for (dpll, &gain) in self.dplls.iter_mut().zip(&self.gains) {
            let demod_out = m_amp - s_amp * gain;
            if let Some(bit) = dpll.step(demod_out) {
                bits.push(bit);
            }
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twist_db_maps_to_gain() {
        // 0 dB is unity; ±6 dB is ÷/× ~2.
        assert!((twist_db_to_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((twist_db_to_gain(-6.0) - 0.5012).abs() < 1e-3);
        assert!((twist_db_to_gain(6.0) - 1.9953).abs() < 1e-3);
    }

    #[test]
    fn gains_span_min_to_max() {
        // Endpoints in dB map to their linear-gain equivalents.
        let g = space_gains(8, -12.0, 9.0);
        assert_eq!(g.len(), 8);
        assert!(
            (g[0] - twist_db_to_gain(-12.0)).abs() < 1e-5,
            "first gain = {}",
            g[0]
        );
        assert!(
            (g[7] - twist_db_to_gain(9.0)).abs() < 1e-5,
            "last gain = {}",
            g[7]
        );
    }

    #[test]
    fn default_ladder_lands_a_rung_on_0db() {
        // The default −12..+9 dB / 8-rung ladder has a 3 dB step, so rung 4 is
        // exactly 0 dB (unity gain).
        let g = space_gains(8, -12.0, 9.0);
        assert!(
            (g[4] - 1.0).abs() < 1e-5,
            "rung 4 = {} (expected unity)",
            g[4]
        );
    }

    #[test]
    fn gains_are_geometric() {
        // Uniform spacing in dB is a geometric progression in linear gain.
        let g = space_gains(8, -12.0, 9.0);
        let ratios: Vec<f32> = g.windows(2).map(|w| w[1] / w[0]).collect();
        let r0 = ratios[0];
        for &r in &ratios {
            assert!((r - r0).abs() < 1e-5, "non-geometric: {r} vs {r0}");
        }
    }

    #[test]
    fn single_slicer_unity_gain() {
        // Unity (0 dB) regardless of the configured range.
        let g = space_gains(1, -12.0, 9.0);
        assert_eq!(g.len(), 1);
        assert!((g[0] - 1.0).abs() < 1e-6);
        assert_eq!(space_gains(1, -3.0, -3.0), vec![1.0]);
    }

    #[test]
    fn slicer_bank_produces_bits() {
        // Drive the bank with a strong mark signal; after 20 samples (one baud
        // period at 24kHz/1200baud) each slicer should have produced one bit.
        let mut bank = SlicerBank::new(8, -12.0, 9.0, 1200, 24000);
        let mut total_bits = 0usize;
        for _ in 0..20 {
            // m_amp >> s_amp → strong mark
            let bits = bank.process(1.0, 0.0);
            total_bits += bits.len();
        }
        // Each of 8 slicers overflows once in 20 samples.
        assert_eq!(total_bits, 8, "expected 8 bits (one per slicer)");
    }
}
