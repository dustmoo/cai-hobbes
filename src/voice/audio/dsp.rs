//! Small pure DSP helpers shared by the audio backends: sample conversion,
//! a streaming linear resampler and a fixed-size framer.

/// f32 in [-1, 1] → i16, clamped.
#[inline]
pub fn f32_to_i16(s: f32) -> i16 {
    let v = (s * 32767.0).round();
    v.clamp(-32768.0, 32767.0) as i16
}

/// i16 → f32 in [-1, 1).
#[inline]
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

/// Linear resampler that keeps its phase and last sample between calls, so
/// arbitrary buffer sizes produce a continuous stream with no drift.
pub struct StreamResampler {
    step: f64,
    /// Read position in "extended" coordinates, where index 0 is `prev` and
    /// index 1 is the first sample of the next input buffer.
    pos: f64,
    prev: f32,
}

impl StreamResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: from_rate as f64 / to_rate as f64,
            pos: 1.0,
            prev: 0.0,
        }
    }

    /// Resample `input`, appending i16 output samples to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        let prev = self.prev;
        let at = |i: usize| if i == 0 { prev } else { input[i - 1] };
        let mut t = self.pos;
        while t < n as f64 {
            let i = t as usize;
            let frac = (t - i as f64) as f32;
            let a = at(i);
            let b = at(i + 1);
            out.push(f32_to_i16(a + (b - a) * frac));
            t += self.step;
        }
        self.pos = t - n as f64;
        self.prev = input[n - 1];
    }
}

/// Collects samples and emits fixed-size frames.
pub struct Framer {
    frame: usize,
    pending: Vec<i16>,
}

impl Framer {
    pub fn new(frame: usize) -> Self {
        Self {
            frame,
            pending: Vec::with_capacity(frame * 4),
        }
    }

    /// Append samples; call `emit` once per complete frame.
    pub fn push(&mut self, samples: &[i16], mut emit: impl FnMut(Vec<i16>)) {
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= self.frame {
            emit(self.pending[start..start + self.frame].to_vec());
            start += self.frame;
        }
        if start > 0 {
            self.pending.drain(..start);
        }
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_i16_roundtrip_and_clamp() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32768);
        assert_eq!(i16_to_f32(-32768), -1.0);
        for s in [-12345i16, -1, 0, 1, 777, 32000] {
            assert!((f32_to_i16(i16_to_f32(s)) as i32 - s as i32).abs() <= 1);
        }
    }

    #[test]
    fn stream_resampler_no_drift_across_odd_buffers() {
        // 1 s at 44.1 kHz in odd-sized chunks must give ~1 s at 16 kHz.
        let mut r = StreamResampler::new(44_100, 16_000);
        let mut out = Vec::new();
        let input = vec![0.25f32; 44_100];
        for chunk in input.chunks(437) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() <= 1, "{}", out.len());
        assert!(out.iter().all(|&s| s == f32_to_i16(0.25)));
    }

    #[test]
    fn stream_resampler_decimates_exactly() {
        let mut r = StreamResampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..480).map(|i| i as f32 / 1000.0).collect();
        let mut out = Vec::new();
        r.process(&input[..100], &mut out);
        r.process(&input[100..], &mut out);
        assert_eq!(out.len(), 160);
        // Picks every 3rd sample.
        assert_eq!(out[5], f32_to_i16(input[15]));
        assert_eq!(out[159], f32_to_i16(input[477]));
    }

    #[test]
    fn framer_emits_fixed_frames_and_keeps_remainder() {
        let mut f = Framer::new(4);
        let mut frames = Vec::new();
        f.push(&[1, 2, 3], |fr| frames.push(fr));
        assert!(frames.is_empty());
        f.push(&[4, 5, 6, 7, 8, 9], |fr| frames.push(fr));
        assert_eq!(frames, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        f.clear();
        f.push(&[10, 11, 12, 13], |fr| frames.push(fr));
        assert_eq!(frames[2], vec![10, 11, 12, 13]);
    }
}
