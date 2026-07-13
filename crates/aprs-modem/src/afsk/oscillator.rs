/// 256-entry cosine lookup table, indexed by the top 8 bits of a 32-bit phase accumulator.
///
/// Matches direwolf's `fcos256_table`:
///   `fcos256_table[j] = cos(j * 2π / 256)`
///
/// The sine of a phase is the cosine shifted by 64 (quarter-period):
///   `fsin256(x) = fcos256_table[((x >> 24) - 64) & 0xff]`
static COS_TABLE: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();

fn cos_table() -> &'static [f32; 256] {
    COS_TABLE.get_or_init(|| {
        let mut t = [0.0f32; 256];
        for (j, v) in t.iter_mut().enumerate() {
            *v = (j as f32 * 2.0 * std::f32::consts::PI / 256.0).cos();
        }
        t
    })
}

/// Look up cosine from the phase accumulator (top 8 bits used as index).
#[inline(always)]
pub fn fcos256(phase: u32) -> f32 {
    cos_table()[((phase >> 24) & 0xff) as usize]
}

/// Look up sine from the phase accumulator (quarter-period shift from cosine).
#[inline(always)]
pub fn fsin256(phase: u32) -> f32 {
    cos_table()[(((phase >> 24).wrapping_sub(64)) & 0xff) as usize]
}

/// A free-running 32-bit phase accumulator local oscillator.
///
/// The phase wraps naturally at 2^32, which is equivalent to 2π.
/// This matches direwolf's `m_osc_phase` / `m_osc_delta` pattern exactly.
///
/// Phase delta is computed as:
///   `delta = round(2^32 * freq / sample_rate)`
#[derive(Debug, Clone)]
pub struct Oscillator {
    pub phase: u32,
    pub delta: u32,
}

impl Oscillator {
    /// Create an oscillator for `freq` Hz at the given `sample_rate`.
    pub fn new(freq: f32, sample_rate: u32) -> Self {
        let delta = (f64::powi(2.0, 32) * freq as f64 / sample_rate as f64).round() as u64;
        Self {
            phase: 0,
            delta: delta as u32,
        }
    }

    /// Advance the phase by one sample and return the new phase.
    #[inline(always)]
    pub fn advance(&mut self) -> u32 {
        self.phase = self.phase.wrapping_add(self.delta);
        self.phase
    }

    /// Cosine of the current phase.
    #[inline(always)]
    pub fn cos(&self) -> f32 {
        fcos256(self.phase)
    }

    /// Sine of the current phase.
    #[inline(always)]
    pub fn sin(&self) -> f32 {
        fsin256(self.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cos_table_at_known_angles() {
        let t = cos_table();
        // Index 0: cos(0) = 1.0
        assert!((t[0] - 1.0).abs() < 1e-6);
        // Index 64: cos(π/2) = 0.0
        assert!(t[64].abs() < 1e-6);
        // Index 128: cos(π) = -1.0
        assert!((t[128] + 1.0).abs() < 1e-6);
        // Index 192: cos(3π/2) = 0.0
        assert!(t[192].abs() < 1e-6);
    }

    #[test]
    fn fsin256_quarter_period_offset() {
        // sin(0) = 0; fsin256(0) uses index (0 - 64) & 0xff = 192, cos_table[192] ≈ 0
        let s = fsin256(0);
        assert!(s.abs() < 1e-6, "fsin256(0) = {s}");
        // fsin256 at phase = 64<<24 → index (64-64)=0, cos_table[0] = 1.0 → sin(π/2) = 1
        let p: u32 = 64u32 << 24;
        let s = fsin256(p);
        assert!((s - 1.0).abs() < 1e-6, "fsin256(π/2) = {s}");
    }

    #[test]
    fn oscillator_frequency_accuracy() {
        // At 24kHz, a 1200 Hz oscillator should complete one full cycle
        // every 24000/1200 = 20 samples. After 20 steps the phase should
        // have advanced by 2^32, wrapping back near zero.
        let mut osc = Oscillator::new(1200.0, 24000);
        for _ in 0..20 {
            osc.advance();
        }
        // After exactly 20 samples the accumulated phase error is:
        // delta = round(2^32 * 1200/24000) = round(2^32 / 20) = 214748365
        // 20 * 214748365 = 4294967300 → wraps to 4 (off by ~4 LSB out of 2^32)
        // The phase error is < 4 / 2^32 ≈ 1 ppm — well within tolerance.
        let phase_frac = osc.phase as f64 / f64::powi(2.0, 32);
        assert!(
            phase_frac < 1e-6,
            "phase residual after 1 cycle = {phase_frac:.2e}"
        );
    }

    #[test]
    fn oscillator_iq_amplitude() {
        // cos² + sin² should stay near 1.0 for any phase.
        let mut osc = Oscillator::new(1200.0, 24000);
        for _ in 0..100 {
            osc.advance();
            let c = osc.cos();
            let s = osc.sin();
            let mag = c * c + s * s;
            assert!((mag - 1.0).abs() < 0.01, "IQ magnitude = {mag}");
        }
    }
}
