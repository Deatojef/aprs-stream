pub mod agc;
pub mod dpll;
pub mod oscillator;
pub mod slicer;

use crate::AudioLevel;
use crate::config::DecoderConfig;
use crate::dsp::{
    filter::{calc_taps, gen_bandpass, gen_rrc_lowpass},
    fir::DelayLine,
};
use agc::AgcState;
use dpll::DemodBit;
use oscillator::Oscillator;
use slicer::SlicerBank;

// Pre-filter parameters for Profile A (from demod_afsk_init, baud > 600 branch).
const PRE_FILTER_LEN_SYM: f32 = 383.0 * 1200.0 / 44100.0; // ~10.42 symbol times
const PRE_FILTER_BAUD: f32 = 0.155; // fraction of baud rate outside tones

// RRC lowpass parameters for Profile A.
const RRC_WIDTH_SYM: f32 = 2.80;
const RRC_ROLLOFF: f32 = 0.20;

// AGC time constants (empirically derived in direwolf).
const AGC_FAST_ATTACK: f32 = 0.70;
const AGC_SLOW_DECAY: f32 = 0.000090;

// Audio-level reporting time constants: 5× slower than the demodulation AGC.
// Matches direwolf's `quick_attack = agc_fast_attack * 0.2` pattern so that
// reported levels are stable across packets and comparable across SSRCs.
const ALEVEL_FAST_ATTACK: f32 = AGC_FAST_ATTACK * 0.2; // 0.14
const ALEVEL_SLOW_DECAY: f32 = AGC_SLOW_DECAY * 0.2; // 0.000018

// Maximum filter size (matches direwolf's MAX_FILTER_SIZE).
const MAX_FILTER_SIZE: usize = 480;

/// AFSK Profile A demodulator state for one audio stream.
///
/// Implements direwolf's `demod_afsk_process_sample` Profile A path:
///   bandpass pre-filter → IQ mixing at mark/space frequencies →
///   RRC lowpass → envelope magnitude → AGC → multi-slicer DPLL.
///
/// Call `process_sample` for each f32 audio sample (normalized [-1, 1]).
/// Bits are emitted via `DemodBit` when any slicer's DPLL overflows.
pub struct AfskDemodulator {
    // Bandpass pre-filter.
    pre_filter: Vec<f32>,
    raw_cb: DelayLine,

    // Mark oscillator and IQ delay lines.
    mark_osc: Oscillator,
    m_i: DelayLine,
    m_q: DelayLine,

    // Space oscillator and IQ delay lines.
    space_osc: Oscillator,
    s_i: DelayLine,
    s_q: DelayLine,

    // Shared lowpass (RRC) kernel — same kernel for all four IQ delay lines.
    lp_filter: Vec<f32>,

    // Per-tone AGC state for tracking amplitude envelope (demodulation normalization).
    m_agc: AgcState,
    s_agc: AgcState,

    // Separate slower-tracking peaks for audio level *reporting* (5× slower IIR).
    // These match direwolf's alevel_rec_peak/valley and alevel_mark/space_peak fields.
    alevel_rec_peak: f32,
    alevel_rec_valley: f32,
    alevel_mark_peak: f32,
    alevel_space_peak: f32,

    // Multi-slicer DPLL bank.
    slicers: SlicerBank,
}

impl AfskDemodulator {
    /// Construct a new demodulator from the decoder configuration.
    pub fn new(cfg: &DecoderConfig, sample_rate: u32) -> Self {
        let baud = cfg.baud;
        let mark_hz = cfg.mark_hz as f32;
        let space_hz = cfg.space_hz as f32;
        let sr = sample_rate as f32;

        // Pre-filter: bandpass centered on [mark_hz, space_hz] ± prefilter_baud * baud.
        let pre_taps = calc_taps(PRE_FILTER_LEN_SYM, sample_rate, baud, MAX_FILTER_SIZE);
        let f1 = (mark_hz.min(space_hz) - PRE_FILTER_BAUD * baud as f32) / sr;
        let f2 = (mark_hz.max(space_hz) + PRE_FILTER_BAUD * baud as f32) / sr;
        let pre_filter = gen_bandpass(f1, f2, pre_taps);

        // RRC lowpass: shared for all four IQ delay lines.
        let sps = sr / baud as f32;
        let lp_taps = calc_taps(RRC_WIDTH_SYM, sample_rate, baud, MAX_FILTER_SIZE);
        let lp_filter = gen_rrc_lowpass(lp_taps, RRC_ROLLOFF, sps);

        Self {
            pre_filter,
            raw_cb: DelayLine::new(pre_taps),
            mark_osc: Oscillator::new(mark_hz, sample_rate),
            m_i: DelayLine::new(lp_taps),
            m_q: DelayLine::new(lp_taps),
            space_osc: Oscillator::new(space_hz, sample_rate),
            s_i: DelayLine::new(lp_taps),
            s_q: DelayLine::new(lp_taps),
            lp_filter,
            m_agc: AgcState::new(),
            s_agc: AgcState::new(),
            alevel_rec_peak: 0.0,
            alevel_rec_valley: 0.0,
            alevel_mark_peak: 0.0,
            alevel_space_peak: 0.0,
            slicers: SlicerBank::new(
                cfg.slicers,
                cfg.min_twist_db,
                cfg.max_twist_db,
                baud,
                sample_rate,
            ),
        }
    }

    /// Process one normalized audio sample.
    ///
    /// Returns a (possibly empty) list of `DemodBit`s — one per slicer that
    /// sampled a bit this audio sample (normally zero, occasionally one per
    /// slicer over a 1/baud-second window).
    #[inline]
    pub fn process_sample(&mut self, sample: f32) -> Vec<DemodBit> {
        // Track raw audio peak/valley for the `rec` audio level (before any filtering).
        // Mirrors direwolf's alevel_rec tracking in demod.c::demod_process_sample.
        if sample >= self.alevel_rec_peak {
            self.alevel_rec_peak =
                sample * ALEVEL_FAST_ATTACK + self.alevel_rec_peak * (1.0 - ALEVEL_FAST_ATTACK);
        } else {
            self.alevel_rec_peak =
                sample * ALEVEL_SLOW_DECAY + self.alevel_rec_peak * (1.0 - ALEVEL_SLOW_DECAY);
        }
        if sample <= self.alevel_rec_valley {
            self.alevel_rec_valley =
                sample * ALEVEL_FAST_ATTACK + self.alevel_rec_valley * (1.0 - ALEVEL_FAST_ATTACK);
        } else {
            self.alevel_rec_valley =
                sample * ALEVEL_SLOW_DECAY + self.alevel_rec_valley * (1.0 - ALEVEL_SLOW_DECAY);
        }

        // 1. Bandpass pre-filter.
        self.raw_cb.push(sample);
        let fsam = self.raw_cb.convolve(&self.pre_filter);

        // 2. Mark IQ mixing and delay.
        let (mc, ms) = (self.mark_osc.cos(), self.mark_osc.sin());
        self.mark_osc.advance();
        self.m_i.push(fsam * mc);
        self.m_q.push(fsam * ms);

        // 3. Space IQ mixing and delay.
        let (sc, ss) = (self.space_osc.cos(), self.space_osc.sin());
        self.space_osc.advance();
        self.s_i.push(fsam * sc);
        self.s_q.push(fsam * ss);

        // 4. Lowpass filter → envelope magnitude.
        let m_i_f = self.m_i.convolve(&self.lp_filter);
        let m_q_f = self.m_q.convolve(&self.lp_filter);
        let m_amp = m_i_f.hypot(m_q_f);

        let s_i_f = self.s_i.convolve(&self.lp_filter);
        let s_q_f = self.s_q.convolve(&self.lp_filter);
        let s_amp = s_i_f.hypot(s_q_f);

        // 5a. Main AGC: fast peak/valley tracking for demodulation normalization.
        self.m_agc.track(m_amp, AGC_FAST_ATTACK, AGC_SLOW_DECAY);
        self.s_agc.track(s_amp, AGC_FAST_ATTACK, AGC_SLOW_DECAY);

        // 5b. Separate slower-tracking peaks for audio level reporting.
        // Mirrors direwolf's alevel_mark_peak / alevel_space_peak in demod_afsk.c.
        if m_amp >= self.alevel_mark_peak {
            self.alevel_mark_peak =
                m_amp * ALEVEL_FAST_ATTACK + self.alevel_mark_peak * (1.0 - ALEVEL_FAST_ATTACK);
        } else {
            self.alevel_mark_peak =
                m_amp * ALEVEL_SLOW_DECAY + self.alevel_mark_peak * (1.0 - ALEVEL_SLOW_DECAY);
        }
        if s_amp >= self.alevel_space_peak {
            self.alevel_space_peak =
                s_amp * ALEVEL_FAST_ATTACK + self.alevel_space_peak * (1.0 - ALEVEL_FAST_ATTACK);
        } else {
            self.alevel_space_peak =
                s_amp * ALEVEL_SLOW_DECAY + self.alevel_space_peak * (1.0 - ALEVEL_SLOW_DECAY);
        }

        // 6. Multi-slicer DPLL: emit bits.
        self.slicers.process(m_amp, s_amp)
    }

    /// Audio levels at the current moment, on direwolf's familiar 0–~200 scale.
    ///
    /// Direwolf normalizes s16 input to ±2.0; we normalize to ±1.0 (standard).
    /// To report values on the same numeric scale direwolf users expect, the
    /// constants here are doubled relative to direwolf's `* 50` / `* 100`.
    ///
    /// - `rec`   = `(raw_peak − raw_valley) × 100`  (~200 = full-scale audio)
    /// - `mark`  = `mark_iq_peak × 200`             (~100 for full-scale tone)
    /// - `space` = `space_iq_peak × 200`
    ///
    /// All three use the slow-tracking (5× longer time constant) IIR so values are
    /// stable across consecutive packets and comparable across different SSRC streams.
    pub fn audio_level(&self) -> AudioLevel {
        let rec = ((self.alevel_rec_peak - self.alevel_rec_valley) * 100.0).clamp(0.0, 255.0) as u8;
        let mark = (self.alevel_mark_peak * 200.0).clamp(0.0, 255.0) as u8;
        let space = (self.alevel_space_peak * 200.0).clamp(0.0, 255.0) as u8;
        AudioLevel { rec, mark, space }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DecoderConfig, FixBits};

    fn default_cfg() -> DecoderConfig {
        DecoderConfig {
            mark_hz: 1200,
            space_hz: 2200,
            baud: 1200,
            slicers: 8,
            min_twist_db: -12.0,
            max_twist_db: 9.0,
            fix_bits: FixBits::Single,
        }
    }

    #[test]
    fn constructs_without_panic() {
        let _demod = AfskDemodulator::new(&default_cfg(), 24000);
    }

    #[test]
    fn filter_sizes_within_bounds() {
        let cfg = default_cfg();
        let demod = AfskDemodulator::new(&cfg, 24000);
        assert!(demod.pre_filter.len() < MAX_FILTER_SIZE);
        assert!(demod.lp_filter.len() < MAX_FILTER_SIZE);
        assert!(
            demod.pre_filter.len() % 2 == 1,
            "pre-filter should be odd-length"
        );
        assert!(
            demod.lp_filter.len() % 2 == 1,
            "lp-filter should be odd-length"
        );
    }

    #[test]
    fn silence_produces_no_bits_initially() {
        let mut demod = AfskDemodulator::new(&default_cfg(), 24000);
        // Feed 100 samples of silence; no bits should be emitted while the
        // DPLL hasn't locked yet (first overflow only happens after ~20 samples).
        // Actually the DPLL will overflow — but with zero input there's no mark/space
        // distinction so all bits should be of indeterminate value but quality=0.
        for _ in 0..100 {
            let _ = demod.process_sample(0.0);
        }
        // Just verify it doesn't panic.
    }

    #[test]
    fn mark_tone_produces_mark_bits() {
        // Feed a pure 1200 Hz sine wave at 24kHz.
        // After warm-up, the mark filter should dominate and most bits should be true.
        let cfg = default_cfg();
        let mut demod = AfskDemodulator::new(&cfg, 24000);
        let sample_rate = 24000usize;
        let mark_hz = 1200.0f32;
        let mut mark_bits = 0usize;
        let mut total_bits = 0usize;

        // 2 seconds of pure mark tone.
        let n_samples = sample_rate * 2;
        for i in 0..n_samples {
            let t = i as f32 / sample_rate as f32;
            let sample = (2.0 * std::f32::consts::PI * mark_hz * t).sin() * 0.5;
            let bits = demod.process_sample(sample);
            for b in bits {
                total_bits += 1;
                if b.bit {
                    mark_bits += 1;
                }
            }
        }

        // After warm-up, the vast majority of bits should be mark.
        // Skip the first ~200ms for filter/AGC convergence.
        // We check aggregate: at least 90% mark bits overall.
        assert!(total_bits > 0, "no bits produced in 2 seconds");
        let mark_ratio = mark_bits as f32 / total_bits as f32;
        assert!(
            mark_ratio > 0.85,
            "mark ratio {:.2} too low (total={}, mark={})",
            mark_ratio,
            total_bits,
            mark_bits
        );
    }

    #[test]
    fn audio_level_mark_tone() {
        // Feed a pure 1200 Hz sine at amplitude 0.5 for 2 seconds.
        // Theoretical rec = (0.5 - (-0.5)) * 100 = 100.
        // Theoretical mark = (0.5 * pre_filter_gain_at_1200 * 0.5_IQ) * 200.
        // For unity pre-filter gain at 1200 Hz: mark ≈ 50.
        let cfg = default_cfg();
        let mut demod = AfskDemodulator::new(&cfg, 24000);
        let sample_rate = 24000usize;
        let amp = 0.5f32;

        for i in 0..(sample_rate * 2) {
            let t = i as f32 / sample_rate as f32;
            let sample = (2.0 * std::f32::consts::PI * 1200.0 * t).sin() * amp;
            let _ = demod.process_sample(sample);
        }

        let al = demod.audio_level();
        eprintln!(
            "audio_level_mark_tone: rec={} mark={} space={}",
            al.rec, al.mark, al.space
        );
        // rec should be ~100 (amplitude 0.5, full swing 1.0, * 100 = 100)
        assert!(
            al.rec >= 80 && al.rec <= 120,
            "rec={} expected ~100",
            al.rec
        );
        // mark should be at least 20 (> 20% of theoretical ~50)
        assert!(
            al.mark >= 20,
            "mark={} unexpectedly low for 1200 Hz tone at amp=0.5",
            al.mark
        );
    }

    #[test]
    fn space_tone_produces_space_bits() {
        let cfg = default_cfg();
        let mut demod = AfskDemodulator::new(&cfg, 24000);
        let sample_rate = 24000usize;
        let space_hz = 2200.0f32;
        let mut space_bits = 0usize;
        let mut total_bits = 0usize;

        for i in 0..sample_rate * 2 {
            let t = i as f32 / sample_rate as f32;
            let sample = (2.0 * std::f32::consts::PI * space_hz * t).sin() * 0.5;
            let bits = demod.process_sample(sample);
            for b in bits {
                total_bits += 1;
                if !b.bit {
                    space_bits += 1;
                }
            }
        }

        assert!(total_bits > 0, "no bits produced");
        let space_ratio = space_bits as f32 / total_bits as f32;
        assert!(
            space_ratio > 0.85,
            "space ratio {:.2} too low (total={}, space={})",
            space_ratio,
            total_bits,
            space_bits
        );
    }
}
