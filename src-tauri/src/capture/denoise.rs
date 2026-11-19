//! Microphone noise suppression for native-engine clips (RNNoise, in Rust).

use nnnoiseless::DenoiseState;

/// Cleans interleaved stereo 48 kHz audio in place. The mic is mono in practice:
/// both channels are averaged, cleaned once, and written back to both.
pub fn stereo(pcm: &mut [i16]) {
    let frames = pcm.len() / 2;
    let n = DenoiseState::FRAME_SIZE;
    let mut st = DenoiseState::new();
    let mut input = vec![0f32; n];
    let mut output = vec![0f32; n];
    let mut first = true;
    let mut pos = 0;
    while pos < frames {
        let len = n.min(frames - pos);
        for i in 0..n {
            input[i] = if i < len { (pcm[2 * (pos + i)] as f32 + pcm[2 * (pos + i) + 1] as f32) * 0.5 } else { 0.0 };
        }
        st.process_frame(&mut output, &input);
        // The first frame is the model warming up: keep it as it was.
        if !first {
            for i in 0..len {
                let v = output[i].clamp(-32768.0, 32767.0) as i16;
                pcm[2 * (pos + i)] = v;
                pcm[2 * (pos + i) + 1] = v;
            }
        }
        first = false;
        pos += len;
    }
}

/// The same, for audio that comes in pieces (a long recording). Pieces should
/// be whole multiples of 480 frames; a shorter last piece is padded.
pub struct Stream {
    st: Box<DenoiseState<'static>>,
    warm: bool,
}

impl Stream {
    pub fn new() -> Self {
        Self { st: DenoiseState::new(), warm: false }
    }

    pub fn process(&mut self, pcm: &mut [i16]) {
        let n = DenoiseState::FRAME_SIZE;
        let frames = pcm.len() / 2;
        let mut input = vec![0f32; n];
        let mut output = vec![0f32; n];
        let mut pos = 0;
        while pos < frames {
            let len = n.min(frames - pos);
            for i in 0..n {
                input[i] = if i < len { (pcm[2 * (pos + i)] as f32 + pcm[2 * (pos + i) + 1] as f32) * 0.5 } else { 0.0 };
            }
            self.st.process_frame(&mut output, &input);
            if self.warm {
                for i in 0..len {
                    let v = output[i].clamp(-32768.0, 32767.0) as i16;
                    pcm[2 * (pos + i)] = v;
                    pcm[2 * (pos + i) + 1] = v;
                }
            }
            self.warm = true;
            pos += len;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quiets_hiss_quickly() {
        // 10 s of hiss.
        let mut seed = 12345u32;
        let mut pcm: Vec<i16> = (0..48_000 * 10 * 2).map(|_| { seed = seed.wrapping_mul(1103515245).wrapping_add(12345); ((seed >> 16) as i16) / 40 }).collect();
        let before: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
        let t = std::time::Instant::now();
        super::stereo(&mut pcm);
        let took = t.elapsed();
        let after: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
        println!("10 s denoised in {took:?}, energy {:.3} of before", after / before);
        assert!(after < before * 0.5);
    }
}
