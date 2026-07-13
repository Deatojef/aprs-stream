/// Per-tone IIR peak/valley envelope tracker (AGC).
///
/// Ported from direwolf's `agc()` in `demod_afsk.c`.
///
/// The peak tracks upward fast and decays slowly; the valley tracks downward
/// fast and rises slowly. This asymmetry means the envelope follows the
/// signal's peaks quickly but releases slowly — appropriate for the bursty
/// nature of APRS transmissions.
///
/// The normalized output is scaled so that the signal swing sits in
/// approximately [-0.5, +0.5], making mark minus space work out to [-1, +1].
#[derive(Debug, Clone)]
pub struct AgcState {
    pub peak: f32,
    pub valley: f32,
}

impl AgcState {
    pub fn new() -> Self {
        Self {
            peak: 0.0,
            valley: 0.0,
        }
    }

    /// Update the envelope and return the normalized value.
    ///
    /// - `fast_attack`: fraction applied when input moves toward the envelope bound (0.70)
    /// - `slow_decay`: fraction applied when input moves away from the envelope bound (0.000090)
    ///
    /// Returns a normalized sample in approximately [-0.5, +0.5].
    /// Returns 0.0 when peak == valley (no signal).
    #[inline(always)]
    pub fn update(&mut self, input: f32, fast_attack: f32, slow_decay: f32) -> f32 {
        // Peak tracking: fast attack when rising, slow decay when falling.
        if input >= self.peak {
            self.peak = input * fast_attack + self.peak * (1.0 - fast_attack);
        } else {
            self.peak = input * slow_decay + self.peak * (1.0 - slow_decay);
        }

        // Valley tracking: fast attack when falling, slow decay when rising.
        if input <= self.valley {
            self.valley = input * fast_attack + self.valley * (1.0 - fast_attack);
        } else {
            self.valley = input * slow_decay + self.valley * (1.0 - slow_decay);
        }

        // Clip input to the envelope before normalizing.
        let x = input.clamp(self.valley, self.peak);

        if self.peak > self.valley {
            (x - 0.5 * (self.peak + self.valley)) / (self.peak - self.valley)
        } else {
            0.0
        }
    }

    /// Update peak/valley tracking only — discard the normalized return value.
    ///
    /// Used in the multi-slicer path where we need the envelope statistics
    /// (peak, valley) for the amplitude/quality calculation but don't need
    /// the normalized output for slicing.
    #[inline(always)]
    pub fn track(&mut self, input: f32, fast_attack: f32, slow_decay: f32) {
        let _ = self.update(input, fast_attack, slow_decay);
    }
}

impl Default for AgcState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_output_before_signal() {
        let mut agc = AgcState::new();
        // With peak == valley == 0, output should be 0.
        let out = agc.update(0.0, 0.70, 0.00009);
        assert_eq!(out, 0.0);
    }

    #[test]
    fn output_settles_near_half() {
        // Feed a constant amplitude signal; output should converge toward 0.5.
        let mut agc = AgcState::new();
        let mut last = 0.0f32;
        for _ in 0..10_000 {
            last = agc.update(1.0, 0.70, 0.00009);
        }
        // After many samples the peak tracks to ~1.0 and valley to ~0.0, so
        // normalized output = (1.0 - 0.5) / (1.0 - 0.0) = 0.5.
        assert!((last - 0.5).abs() < 0.01, "settled value = {last}");
    }

    #[test]
    fn fast_attack_on_rising_input() {
        let mut agc = AgcState::new();
        // After a burst of high-amplitude input the peak should rise quickly.
        for _ in 0..5 {
            agc.update(1.0, 0.70, 0.00009);
        }
        // fast_attack = 0.70, so after 5 steps: peak ≈ 1 - (1-0.7)^5 ≈ 0.9976
        assert!(agc.peak > 0.99, "peak = {}", agc.peak);
    }

    #[test]
    fn slow_decay_persists_after_signal_stops() {
        let mut agc = AgcState::new();
        // Build up the peak.
        for _ in 0..1_000 {
            agc.update(1.0, 0.70, 0.00009);
        }
        let peak_before = agc.peak;
        // 100 samples of silence.
        for _ in 0..100 {
            agc.update(0.0, 0.70, 0.00009);
        }
        // With slow_decay = 0.00009, peak should barely have moved.
        assert!(
            (agc.peak - peak_before).abs() < 0.02,
            "peak decayed too fast"
        );
    }
}
