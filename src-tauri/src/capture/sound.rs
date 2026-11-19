//! Short chimes so you know a clip was saved without leaving the game.

use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub enum Cue {
    Saved,
    RecordStart,
    RecordStop,
    Error,
}

/// A 16-bit mono WAV of a few soft sine notes.
pub fn wav(notes: &[(f32, f32)], volume: f32) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, secs) in notes {
        let n = (secs * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            // Quick attack, smooth decay: no clicks.
            let env = (t / 0.008).min(1.0) * (1.0 - i as f32 / n as f32).powf(2.0);
            pcm.push(((2.0 * std::f32::consts::PI * freq * t).sin() * env * volume * 32767.0) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn sound(cue: Cue) -> &'static [u8] {
    static SAVED: OnceLock<Vec<u8>> = OnceLock::new();
    static START: OnceLock<Vec<u8>> = OnceLock::new();
    static STOP: OnceLock<Vec<u8>> = OnceLock::new();
    static ERROR: OnceLock<Vec<u8>> = OnceLock::new();
    match cue {
        Cue::Saved => SAVED.get_or_init(|| wav(&[(880.0, 0.09), (1318.5, 0.16)], 0.35)),
        Cue::RecordStart => START.get_or_init(|| wav(&[(659.3, 0.08), (987.8, 0.14)], 0.35)),
        Cue::RecordStop => STOP.get_or_init(|| wav(&[(987.8, 0.08), (659.3, 0.14)], 0.35)),
        Cue::Error => ERROR.get_or_init(|| wav(&[(220.0, 0.12), (196.0, 0.18)], 0.4)),
    }
}

/// Plays a moment of silence so Windows has the sound device open before the
/// first real chime (the first PlaySound is noticeably slower).
pub fn warm() {
    std::thread::spawn(|| {
        for c in [Cue::Saved, Cue::RecordStart, Cue::RecordStop, Cue::Error] {
            let _ = sound(c);
        }
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
            static SILENCE: OnceLock<Vec<u8>> = OnceLock::new();
            let data = SILENCE.get_or_init(|| wav(&[(440.0, 0.02)], 0.0));
            let _ = PlaySoundW(windows::core::PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_SYNC | SND_NODEFAULT);
        }
    });
}

pub fn play(cue: Cue) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
        let data = sound(cue);
        let _ = PlaySoundW(windows::core::PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_header() {
        let w = super::wav(&[(440.0, 0.1)], 0.5);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize, w.len() - 8);
        assert_eq!(w.len(), 44 + 4410 * 2);
    }
}
