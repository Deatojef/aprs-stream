use std::f32::consts::PI;

/// Generate a bandpass FIR kernel, normalized for unity gain at the center of the passband.
///
/// Used as the pre-filter that attenuates frequencies outside the mark/space band.
///
/// - `f1`, `f2`: lower and upper cutoff frequencies as fractions of the sample rate
/// - `size`: number of taps (should be odd)
pub fn gen_bandpass(f1: f32, f2: f32, size: usize) -> Vec<f32> {
    assert!(size >= 3, "filter size must be >= 3");
    let center = 0.5 * (size as f32 - 1.0);
    let mut out: Vec<f32> = (0..size)
        .map(|j| {
            let j = j as f32;
            if (j - center).abs() < 1e-6 {
                2.0 * (f2 - f1)
            } else {
                (2.0 * PI * f2 * (j - center)).sin() / (PI * (j - center))
                    - (2.0 * PI * f1 * (j - center)).sin() / (PI * (j - center))
            }
        })
        .collect();

    // Normalize for unity gain at the center of the passband.
    // The gain at ω_c for a symmetric FIR is G(ω_c) = Σ h[n]·cos(ω_c·(n−center)).
    // Dividing by that sum gives |H(ω_c)| = 1.0 for a cosine input at ω_c.
    let w = 2.0 * PI * (f1 + f2) / 2.0;
    let g: f32 = out
        .iter()
        .enumerate()
        .map(|(j, &v)| v * ((j as f32 - center) * w).cos())
        .sum();
    out.iter_mut().for_each(|v| *v /= g);
    out
}

/// Root Raised Cosine (RRC) function at normalized time `t` with rolloff factor `a`.
///
/// `t` is in symbol durations; the kernel is centered at t=0.
/// At t=0 the result is 1.0; at all other integer t the result is ~0.
fn rrc(t: f32, a: f32) -> f32 {
    let sinc = if t.abs() < 1e-3 {
        1.0
    } else {
        (PI * t).sin() / (PI * t)
    };

    let at = a * t;
    let win = if (at.abs() - 0.5).abs() < 1e-3 {
        PI / 4.0
    } else {
        (PI * at).cos() / (1.0 - (2.0 * at).powi(2))
    };

    sinc * win
}

/// Generate a Root Raised Cosine (RRC) lowpass FIR kernel, normalized for unity gain.
///
/// RRC filters minimize intersymbol interference (ISI) compared to a plain lowpass.
/// This is direwolf's preferred filter for Profile A.
///
/// - `size`: number of taps (should be odd)
/// - `rolloff`: rolloff factor in [0, 1]; direwolf uses 0.20 for Profile A
/// - `samples_per_symbol`: sample rate / baud rate
pub fn gen_rrc_lowpass(size: usize, rolloff: f32, samples_per_symbol: f32) -> Vec<f32> {
    assert!(size >= 3, "filter size must be >= 3");
    let half = (size as f32 - 1.0) / 2.0;
    let mut out: Vec<f32> = (0..size)
        .map(|k| {
            let t = (k as f32 - half) / samples_per_symbol;
            rrc(t, rolloff)
        })
        .collect();

    // Normalize for unity gain.
    let g: f32 = out.iter().sum();
    out.iter_mut().for_each(|v| *v /= g);
    out
}

/// Compute the number of filter taps for a pre-filter or lowpass filter.
///
/// The result is forced odd (better symmetry) and clamped to `max_taps`.
/// Mirrors the calculation in `demod_afsk_init`.
pub fn calc_taps(width_sym: f32, sample_rate: u32, baud: u32, max_taps: usize) -> usize {
    let raw = (width_sym * sample_rate as f32 / baud as f32) as usize;
    let odd = raw | 1; // force odd
    odd.min((max_taps - 1) | 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrc_lowpass_unity_gain() {
        // 24000 Hz / 1200 baud = 20 samples per symbol; 2.80 sym width → 56 taps → 57 (odd)
        let sps: f32 = 24000.0 / 1200.0;
        let taps = ((2.80 * sps) as usize) | 1;
        let kernel = gen_rrc_lowpass(taps, 0.20, sps);
        let dc_gain: f32 = kernel.iter().sum();
        assert!((dc_gain - 1.0).abs() < 1e-4, "RRC DC gain = {dc_gain}");
    }

    #[test]
    fn bandpass_nonzero_in_passband() {
        // Pre-filter for 1200/2200 Hz at 24kHz with prefilter_baud=0.155, baud=1200
        let baud = 1200.0f32;
        let sps = 24000.0f32;
        let f1 = (1200.0 - 0.155 * baud) / sps;
        let f2 = (2200.0 + 0.155 * baud) / sps;
        let kernel = gen_bandpass(f1, f2, 63);
        // Gain at center: G(fc) = Σ h[n]·cos(2π·fc·(n−center)).
        // For a cosine input at fc, the output amplitude = G(fc); should be ~1.0.
        let fc = (f1 + f2) / 2.0;
        let center = 31.0f32;
        let gain: f32 = kernel
            .iter()
            .enumerate()
            .map(|(j, &v)| v * ((j as f32 - center) * 2.0 * PI * fc).cos())
            .sum();
        assert!((gain - 1.0).abs() < 0.05, "bandpass center gain = {gain}");
    }

    #[test]
    fn pre_filter_tap_count_at_24khz() {
        // Profile A: pre_filter_len_sym = 383 * 1200/44100 ≈ 10.42 sym
        // At 24kHz/1200baud: 10.42 * 24000/1200 = 208.3 → 209 (odd)
        let width_sym = 383.0 * 1200.0 / 44100.0;
        let taps = calc_taps(width_sym, 24000, 1200, 480);
        assert!(taps % 2 == 1, "should be odd");
        assert!(taps < 480, "should fit in MAX_FILTER_SIZE");
        assert!(taps > 8, "should have enough taps");
    }
}
