/// A demodulated bit emitted when the DPLL overflows.
#[derive(Debug, Clone, Copy)]
pub struct DemodBit {
    /// The bit value: true = mark, false = space.
    pub bit: bool,
    /// Slicer index that produced this bit (0..num_slicers).
    pub slice: usize,
}

// DCD thresholds (from fsk_demod_state.h).
// score is a 32-bit history; popcount >= ON means locked, <= OFF means unlocked.
const DCD_THRESH_ON: u32 = 30;
const DCD_THRESH_OFF: u32 = 6;
// A transition is "good" if it occurs within ±DCD_GOOD_WIDTH * 2^20 of zero phase.
const DCD_GOOD_HALF_WIDTH: i32 = 512 * 1024 * 1024; // 0x2000_0000

/// Digital Phase-Locked Loop for AFSK bit timing recovery.
///
/// Ported from `nudge_pll` + `pll_dcd_*` in direwolf's `demod_afsk.c` /
/// `fsk_demod_state.h`.
///
/// The 32-bit accumulator advances by `step` each sample.  When it overflows
/// from positive to negative (treating its value as signed) a bit is sampled.
/// On each demodulator output transition the phase is nudged toward zero,
/// providing clock recovery without a dedicated crystal reference.
#[derive(Debug, Clone)]
pub struct Dpll {
    /// Signed accumulator; sampled when it wraps positive→negative.
    data_clock_pll: i32,
    prev_d_c_pll: i32,
    /// Added to accumulator each audio sample (unsigned arithmetic, no UB).
    step: u32,
    /// Previous demodulated bit for transition detection.
    prev_demod_data: bool,
    /// Fraction applied when PLL is locked (less aggressive nudge).
    locked_inertia: f32,
    /// Fraction applied when PLL is searching (more aggressive nudge).
    searching_inertia: f32,
    // DCD state.
    good_flag: bool,
    bad_flag: bool,
    good_hist: u8,
    bad_hist: u8,
    score: u32,
    pub data_detect: bool,
    /// Which slicer slot this DPLL belongs to (passed through to DemodBit).
    slice: usize,
}

impl Dpll {
    /// Create a DPLL for the given baud rate and sample rate.
    ///
    /// `pll_step = round(2^32 * baud / sample_rate)`
    pub fn new(baud: u32, sample_rate: u32, slice: usize) -> Self {
        let step = ((f64::powi(2.0, 32) * baud as f64 / sample_rate as f64).round() as u64) as u32;
        Self {
            data_clock_pll: 0,
            prev_d_c_pll: 0,
            step,
            prev_demod_data: false,
            locked_inertia: 0.74,
            searching_inertia: 0.50,
            good_flag: false,
            bad_flag: false,
            good_hist: 0,
            bad_hist: 0,
            score: 0,
            data_detect: false,
            slice,
        }
    }

    /// Advance the DPLL by one audio sample.
    ///
    /// `demod_out`: raw demodulated value (positive = mark, negative = space).
    ///
    /// Returns `Some(DemodBit)` when the accumulator overflows (bit sampling event),
    /// or `None` most of the time.
    #[inline]
    pub fn step(&mut self, demod_out: f32) -> Option<DemodBit> {
        self.prev_d_c_pll = self.data_clock_pll;

        // Unsigned add to avoid signed-integer overflow UB (mirrors direwolf's cast).
        self.data_clock_pll = (self.data_clock_pll as u32).wrapping_add(self.step) as i32;

        let bit_sampled = if self.data_clock_pll < 0 && self.prev_d_c_pll > 0 {
            self.dcd_each_symbol();
            Some(DemodBit {
                bit: demod_out > 0.0,
                slice: self.slice,
            })
        } else {
            None
        };

        // Transition detection: nudge phase toward zero on demod transitions.
        let demod_data = demod_out > 0.0;
        if demod_data != self.prev_demod_data {
            self.dcd_signal_transition(self.data_clock_pll);
            let inertia = if self.data_detect {
                self.locked_inertia
            } else {
                self.searching_inertia
            };
            self.data_clock_pll = (self.data_clock_pll as f32 * inertia) as i32;
        }
        self.prev_demod_data = demod_data;

        bit_sampled
    }

    /// Record a signal transition; classify as good or bad relative to phase.
    #[inline(always)]
    fn dcd_signal_transition(&mut self, dpll_phase: i32) {
        if dpll_phase > -DCD_GOOD_HALF_WIDTH && dpll_phase < DCD_GOOD_HALF_WIDTH {
            self.good_flag = true;
        } else {
            self.bad_flag = true;
        }
    }

    /// Update DCD score at each bit-sample event (one per symbol).
    #[inline(always)]
    fn dcd_each_symbol(&mut self) {
        self.good_hist = (self.good_hist << 1) | (self.good_flag as u8);
        self.good_flag = false;

        self.bad_hist = (self.bad_hist << 1) | (self.bad_flag as u8);
        self.bad_flag = false;

        let good_count = self.good_hist.count_ones() as i32;
        let bad_count = self.bad_hist.count_ones() as i32;
        let win = (good_count - bad_count >= 2) as u32;
        self.score = (self.score << 1) | win;

        let s = self.score.count_ones();
        if s >= DCD_THRESH_ON {
            self.data_detect = true;
        } else if s <= DCD_THRESH_OFF {
            self.data_detect = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_advances_and_wraps() {
        // At 1200 baud / 24000 Hz: step = round(2^32 / 20) = 214748365
        // After 20 steps the accumulator wraps once → one bit sampled.
        let mut dpll = Dpll::new(1200, 24000, 0);
        let mut bits = 0usize;
        for _ in 0..20 {
            if dpll.step(1.0).is_some() {
                bits += 1;
            }
        }
        assert_eq!(bits, 1, "expected exactly 1 bit overflow in 20 samples");
    }

    #[test]
    fn transitions_nudge_phase() {
        // A demod transition should reduce |data_clock_pll| (nudge toward 0).
        let mut dpll = Dpll::new(1200, 24000, 0);
        dpll.data_clock_pll = 0x4000_0000; // large positive, far from sample point
        dpll.prev_demod_data = false;
        // Transition: prev=false, new=true with demod_out > 0
        dpll.step(1.0); // should nudge the pll
        // After a transition, pll should have been multiplied by searching_inertia (0.50)
        // from where it was after the step increment. Hard to check exactly, but it should
        // be less than the starting value + step.
        assert!(dpll.data_clock_pll < 0x4000_0000 + dpll.step as i32);
    }

    #[test]
    fn dcd_detects_lock_after_good_transitions() {
        // Feed consistent transitions near phase 0; DCD should eventually lock.
        let mut dpll = Dpll::new(1200, 24000, 0);
        // Simulate 40 symbol periods worth of good transitions.
        // Each symbol = 20 samples at 24kHz/1200baud; feed one transition per symbol.
        for i in 0..40 {
            // Near phase 0 (good transition) at each symbol boundary
            dpll.good_flag = true;
            dpll.dcd_each_symbol();
            let _ = i;
        }
        assert!(dpll.data_detect, "should be locked after 40 good symbols");
    }
}
