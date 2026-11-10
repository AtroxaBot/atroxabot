//! Loudness and sample-rate conversion.

/// Root mean square of the samples, on the `i16` scale (0 to 32768).
pub(crate) fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&sample| f64::from(sample) * f64::from(sample)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Streaming linear resampler for mono audio; chunk boundaries do not show in the output.
// ponytail: linear interpolation, no anti-alias filter. Fine for speech; switch to a windowed-sinc
// filter if recognition of sibilants suffers on 44.1/48 kHz microphones.
pub(crate) struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample lies, counted from the last sample of the previous chunk.
    position: f64,
    last: Option<i16>,
    same_rate: bool,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32) -> Self {
        let step = f64::from(from.max(1)) / f64::from(to.max(1));
        Self { step, position: 1.0, last: None, same_rate: from == to }
    }

    /// Appends the resampled `input` to `output`.
    pub(crate) fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        let Some(&first) = input.first() else { return };
        if self.same_rate {
            output.extend_from_slice(input);
            return;
        }
        // Index 0 is the previous chunk's last sample, index n is input[n - 1].
        let last = self.last.unwrap_or(first);
        let len = input.len() as f64;
        while self.position < len {
            let index = self.position as usize;
            let before = index.checked_sub(1).and_then(|i| input.get(i)).copied().unwrap_or(last);
            let after = input.get(index).copied().unwrap_or(before);
            let fraction = self.position - index as f64;
            output.push((f64::from(before) + (f64::from(after) - f64::from(before)) * fraction).round() as i16);
            self.position += self.step;
        }
        self.position -= len;
        self.last = input.last().copied();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resample(from: u32, to: u32, chunks: &[&[i16]]) -> Vec<i16> {
        let mut resampler = Resampler::new(from, to);
        let mut output = Vec::new();
        for chunk in chunks {
            resampler.process(chunk, &mut output);
        }
        output
    }

    #[test]
    fn rms_of_silence_a_constant_and_nothing() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0; 480]), 0.0);
        assert!((rms(&[1000; 480]) - 1000.0).abs() < 0.01);
        assert!((rms(&[3, -4, 3, -4]) - 3.535).abs() < 0.01);
    }

    #[test]
    fn same_rate_passes_through() {
        assert_eq!(resample(16_000, 16_000, &[&[1, 2, 3], &[4]]), [1, 2, 3, 4]);
    }

    #[test]
    fn downsampling_a_ramp_keeps_the_ramp() {
        let ramp: Vec<i16> = (0..4800).collect();
        let output = resample(48_000, 16_000, &[&ramp]);
        assert_eq!(output.len(), 1600);
        assert!(output.iter().enumerate().all(|(i, &sample)| sample == i as i16 * 3));
    }

    #[test]
    fn upsampling_interpolates_between_samples() {
        // The last input sample comes out once the next chunk shows what follows it.
        assert_eq!(resample(24_000, 48_000, &[&[0, 100, 200, 100]]), [0, 50, 100, 150, 200, 150]);
        assert_eq!(resample(24_000, 48_000, &[&[0, 100, 200, 100], &[100]]), [0, 50, 100, 150, 200, 150, 100, 100]);
    }

    #[test]
    fn non_integer_ratio_gives_the_expected_length() {
        let input = vec![0i16; 44_100];
        let output = resample(44_100, 16_000, &[&input]);
        assert!(output.len().abs_diff(16_000) <= 1);
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let wave: Vec<i16> = (0..2000).map(|i| ((f64::from(i) * 0.05).sin() * 10_000.0) as i16).collect();
        for (from, to) in [(48_000, 16_000), (44_100, 16_000), (24_000, 48_000), (24_000, 44_100)] {
            let whole = resample(from, to, &[&wave]);
            let split = resample(from, to, &[&wave[..7], &wave[7..8], &wave[8..1001], &[], &wave[1001..]]);
            // The position is re-based per chunk, so rounding may differ by one step, never by a sample.
            assert_eq!(whole.len(), split.len(), "{from} -> {to}");
            assert!(whole.iter().zip(&split).all(|(a, b)| a.abs_diff(*b) <= 1), "{from} -> {to}");
        }
    }
}
