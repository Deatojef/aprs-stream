/// Fixed-capacity circular delay line for FIR filter convolution.
///
/// Samples are pushed in at the head; the oldest sample falls off the tail.
/// The layout keeps the most recent sample at index 0, matching direwolf's
/// `push_sample` / `convolve` convention.
///
/// Unlike direwolf's `memmove`-based approach, this uses a true ring buffer
/// so each push is O(1) with no data movement.
#[rustfmt::skip]
pub struct DelayLine {
    buf: Vec<f32>,
    head: usize, // index of the most recently written sample
    len: usize,  // number of taps (capacity)
}

impl DelayLine {
    /// Create a new delay line of `taps` samples, initialized to zero.
    pub fn new(taps: usize) -> Self {
        assert!(taps > 0);
        Self {
            buf: vec![0.0f32; taps],
            head: 0,
            len: taps,
        }
    }

    /// Push one new sample, displacing the oldest.
    #[inline(always)]
    pub fn push(&mut self, val: f32) {
        // head advances backward (wrapping); index 0 maps to head.
        self.head = self.head.wrapping_sub(1).min(self.len - 1);
        // The wrapping_sub of 0 gives usize::MAX, .min(len-1) clamps it correctly.
        self.buf[self.head] = val;
    }

    /// Convolve the delay line contents with `kernel`.
    ///
    /// `kernel[0]` multiplies the most-recent sample; `kernel[len-1]` multiplies
    /// the oldest — identical to direwolf's `filter[j] * data[j]` ordering.
    #[inline(always)]
    pub fn convolve(&self, kernel: &[f32]) -> f32 {
        assert_eq!(
            kernel.len(),
            self.len,
            "kernel length must match delay line"
        );
        let mut sum = 0.0f32;
        let split = self.len - self.head;
        // The ring is laid out as: [head..len] is the newest portion, [0..head] the oldest.
        // kernel[0..split] maps to buf[head..len], kernel[split..] maps to buf[0..head].
        for (k, v) in kernel[..split].iter().zip(&self.buf[self.head..]) {
            sum += k * v;
        }
        for (k, v) in kernel[split..].iter().zip(&self.buf[..self.head]) {
            sum += k * v;
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_convolve_identity() {
        // Identity kernel: [1, 0, 0] — output should equal the most-recent sample.
        let mut dl = DelayLine::new(3);
        let kernel = [1.0f32, 0.0, 0.0];
        dl.push(1.0);
        assert!((dl.convolve(&kernel) - 1.0).abs() < 1e-7);
        dl.push(2.0);
        assert!((dl.convolve(&kernel) - 2.0).abs() < 1e-7);
        dl.push(3.0);
        assert!((dl.convolve(&kernel) - 3.0).abs() < 1e-7);
    }

    #[test]
    fn delay_kernel_gives_previous_sample() {
        // Kernel [0, 1, 0] — output is sample from one step ago.
        let mut dl = DelayLine::new(3);
        let kernel = [0.0f32, 1.0, 0.0];
        dl.push(1.0);
        dl.push(2.0);
        assert!((dl.convolve(&kernel) - 1.0).abs() < 1e-7);
        dl.push(3.0);
        assert!((dl.convolve(&kernel) - 2.0).abs() < 1e-7);
    }

    #[test]
    fn averaging_filter_three_taps() {
        // Kernel [1/3, 1/3, 1/3] — running average over 3 samples.
        let mut dl = DelayLine::new(3);
        let k = 1.0f32 / 3.0;
        let kernel = [k, k, k];
        dl.push(3.0);
        dl.push(6.0);
        dl.push(9.0);
        // avg(9, 6, 3) = 6.0
        assert!((dl.convolve(&kernel) - 6.0).abs() < 1e-5);
    }

    #[test]
    fn matches_naive_convolution() {
        // Compare ring-buffer result against a naive O(n) implementation.
        let taps = 31;
        let kernel: Vec<f32> = (0..taps).map(|i| i as f32 * 0.03).collect();
        let mut dl = DelayLine::new(taps);

        let samples: Vec<f32> = (0..50).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut reference = vec![0.0f32; taps]; // most-recent first

        for &s in &samples {
            // Shift reference buffer
            reference.copy_within(0..taps - 1, 1);
            reference[0] = s;

            dl.push(s);

            let naive: f32 = kernel.iter().zip(&reference).map(|(k, v)| k * v).sum();
            let fast = dl.convolve(&kernel);
            assert!((fast - naive).abs() < 1e-5, "fast={fast} naive={naive}");
        }
    }
}
