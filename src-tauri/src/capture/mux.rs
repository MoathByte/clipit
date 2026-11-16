//! Turns buffered video + raw audio into an MP4.
//!
//! Disk-friendly by design: video is copied (not re-encoded), audio is encoded
//! only now, and the file is written once, front to back. The MP4 index ("moov")
//! goes into space reserved at the start (-moov_size) instead of the usual
//! "+faststart", which writes the whole file a second time.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use super::encoder::CREATE_NO_WINDOW;
use super::ts::Snapshot;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Clip saves and the recording muxer run at the game's own (normal) priority.
/// At below-normal a busy game starved them: a 15 s clip took ~20 s to save
/// with every core in use, instead of half a second. They're short bursts
/// (copying video, encoding a little audio), so the game barely notices.
const SAVE_PRIORITY: u32 = 0x0000_0020; // NORMAL_PRIORITY_CLASS

#[derive(Clone, Debug, PartialEq)]
pub struct AudioOpts {
    pub desktop: bool,
    pub mic: bool,
    pub desktop_volume: u32,
    pub mic_volume: u32,
    pub noise_suppression: bool,
    pub separate_tracks: bool,
}

impl AudioOpts {
    pub fn channels(&self) -> u32 {
        (self.desktop as u32 + self.mic as u32) * 2
    }
}

/// ffmpeg arguments after the inputs: audio filters, mapping, codecs.
pub fn audio_args(a: &AudioOpts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let vol = |v: u32| format!("volume={:.2}", v as f64 / 100.0);
    let mic_chain = |pad: &str| {
        let mut s = format!("{pad}{}", vol(a.mic_volume));
        if a.noise_suppression {
            s.push_str(",afftdn=nf=-25");
        }
        s
    };
    let mut maps: Vec<(String, &str)> = Vec::new();
    let graph = match (a.desktop, a.mic) {
        (true, true) => {
            let mut g = format!(
                "[1:a]asplit=2[x][y];[x]pan=stereo|c0=c0|c1=c1,{}[d];{}[m];",
                vol(a.desktop_volume),
                mic_chain("[y]pan=stereo|c0=c2|c1=c3,")
            );
            if a.separate_tracks {
                g.push_str("[d]asplit=2[d1][d2];[m]asplit=2[m1][m2];[d1][m1]amix=inputs=2:normalize=0:duration=longest,alimiter=limit=0.97[mix]");
                maps.push(("[mix]".into(), "Game + mic"));
                maps.push(("[d2]".into(), "Desktop"));
                maps.push(("[m2]".into(), "Microphone"));
            } else {
                g.push_str("[d][m]amix=inputs=2:normalize=0:duration=longest,alimiter=limit=0.97[mix]");
                maps.push(("[mix]".into(), "Game + mic"));
            }
            Some(g)
        }
        (true, false) => {
            maps.push(("[d]".into(), "Desktop"));
            Some(format!("[1:a]{}[d]", vol(a.desktop_volume)))
        }
        (false, true) => {
            maps.push(("[m]".into(), "Microphone"));
            Some(mic_chain("[1:a]") + "[m]")
        }
        (false, false) => None,
    };
    out.extend(["-map".into(), "0:v".into()]);
    if let Some(g) = graph {
        out.extend(["-filter_complex".into(), g]);
        for (i, (pad, title)) in maps.iter().enumerate() {
            out.extend(["-map".into(), pad.clone()]);
            out.extend([format!("-metadata:s:a:{i}"), format!("title={title}")]);
        }
        out.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
    }
    out.extend(["-c:v".into(), "copy".into()]);
    out
}

/// Space to reserve for the MP4 index: ~12 bytes per video frame and per audio
/// frame per track, plus headroom.
pub fn moov_size(secs: f64, fps: u32, tracks: u32) -> u64 {
    let frames = secs.max(1.0) * fps as f64 + secs.max(1.0) * 47.0 * tracks as f64;
    (frames * 12.0) as u64 + 64 * 1024
}

/// Interleaves desktop and mic stereo PCM into one 2- or 4-channel stream.
pub fn interleave(desktop: Option<&[i16]>, mic: Option<&[i16]>) -> Vec<u8> {
    match (desktop, mic) {
        (Some(d), Some(m)) => {
            let frames = (d.len() / 2).min(m.len() / 2);
            let mut out = Vec::with_capacity(frames * 8);
            for f in 0..frames {
                for s in [d[2 * f], d[2 * f + 1], m[2 * f], m[2 * f + 1]] {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
            out
        }
        (Some(x), None) | (None, Some(x)) => x.iter().flat_map(|s| s.to_le_bytes()).collect(),
        (None, None) => Vec::new(),
    }
}

// ---------- named pipe for the audio input ----------

static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A one-shot named pipe that ffmpeg opens as an input file.
pub struct Pipe {
    pub name: String,
    #[cfg(windows)]
    handle: isize,
}

impl Pipe {
    #[cfg(windows)]
    pub fn new() -> Result<Self, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = format!(r"\\.\pipe\clipit-{}-{}", std::process::id(), PIPE_SEQ.fetch_add(1, Ordering::SeqCst));
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let h = unsafe { CreateNamedPipeW(PCWSTR(wide.as_ptr()), PIPE_ACCESS_OUTBOUND, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 1 << 20, 0, 0, None) };
        if h.is_invalid() {
            return Err("couldn't create audio pipe".into());
        }
        Ok(Self { name, handle: h.0 as isize })
    }

    /// A pipe ffmpeg writes into (the killfeed tap); read it with `connect_reader`.
    #[cfg(windows)]
    pub fn new_inbound() -> Result<Self, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = format!(r"\\.\pipe\clipit-{}-{}", std::process::id(), PIPE_SEQ.fetch_add(1, Ordering::SeqCst));
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let h = unsafe { CreateNamedPipeW(PCWSTR(wide.as_ptr()), PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 0, 4 << 20, 0, None) };
        if h.is_invalid() {
            return Err("couldn't create the killfeed pipe".into());
        }
        Ok(Self { name, handle: h.0 as isize })
    }

    /// Waits for ffmpeg to open the pipe for writing, then hands back a readable file.
    #[cfg(windows)]
    pub fn connect_reader(self) -> Result<std::fs::File, String> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        let h = HANDLE(self.handle as *mut _);
        if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                return Err(e.to_string());
            }
        }
        let file = unsafe { std::fs::File::from_raw_handle(self.handle as *mut _) };
        std::mem::forget(self);
        Ok(file)
    }

    /// Waits for ffmpeg to open the pipe, then hands back a writable file.
    #[cfg(windows)]
    pub fn connect(self) -> Result<PipeWriter, String> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        let h = HANDLE(self.handle as *mut _);
        if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                unsafe { let _ = windows::Win32::Foundation::CloseHandle(h); }
                return Err(e.to_string());
            }
        }
        let file = unsafe { std::fs::File::from_raw_handle(self.handle as *mut _) };
        std::mem::forget(self);
        Ok(PipeWriter { file })
    }

    /// Unblocks a pending `connect` (used when ffmpeg died before opening it).
    pub fn poke(name: &str) {
        let _ = std::fs::OpenOptions::new().read(true).open(name);
    }

    /// Unblocks a pending `connect_reader` (ffmpeg died before opening it).
    pub fn poke_writer(name: &str) {
        let _ = std::fs::OpenOptions::new().write(true).open(name);
    }
}

#[cfg(windows)]
impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(self.handle as *mut _)); }
    }
}

pub struct PipeWriter {
    file: std::fs::File,
}

impl PipeWriter {
    pub fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(data)
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        // Let ffmpeg read everything before the pipe closes.
        let _ = self.file.flush();
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::Storage::FileSystem::FlushFileBuffers(windows::Win32::Foundation::HANDLE(self.file.as_raw_handle() as *mut _));
        }
    }
}

// ---------- ffmpeg process ----------

fn base_cmd(ffmpeg: &Path) -> Command {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-nostdin"]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | SAVE_PRIORITY);
    cmd
}

fn audio_input(cmd: &mut Command, pipe: &str, channels: u32) {
    cmd.args(["-f", "s16le", "-ar", "48000", "-ac", &channels.to_string(), "-i", pipe]);
}

fn finish(child: Child, part: &Path, out: &Path) -> Result<u64, String> {
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let _ = std::fs::remove_file(part);
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(err.lines().last().unwrap_or("ffmpeg failed").trim().to_string());
    }
    std::fs::rename(part, out).map_err(|e| e.to_string())?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

/// The ".part" name a clip is written under until it's complete.
pub fn part_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Writes a replay clip. `pcm` = interleaved 2/4-channel s16 audio matching the video.
pub fn save_clip(
    ffmpeg: &Path,
    snap: &Snapshot,
    pcm: Vec<u8>,
    audio: &AudioOpts,
    fps: u32,
    out: &Path,
    register: &dyn Fn(&Child),
) -> Result<u64, String> {
    let secs = (snap.end_hns() - snap.start_hns()) as f64 / 1e7;
    let tracks = if audio.channels() == 0 { 0 } else if audio.desktop && audio.mic && audio.separate_tracks { 3 } else { 1 };
    let attempt = |faststart: bool| -> Result<u64, String> {
        let part = part_path(out);
        let pipe = if audio.channels() > 0 { Some(Pipe::new()?) } else { None };
        let mut cmd = base_cmd(ffmpeg);
        cmd.args(["-f", "mpegts", "-i", "pipe:0"]);
        if let Some(p) = &pipe {
            audio_input(&mut cmd, &p.name, audio.channels());
        }
        cmd.args(audio_args(audio));
        if faststart {
            cmd.args(["-movflags", "+faststart"]);
        } else {
            cmd.args(["-moov_size", &moov_size(secs, fps, tracks).to_string()]);
        }
        cmd.args(["-f", "mp4"]).arg(&part);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        register(&child);
        let mut stdin = child.stdin.take().unwrap();
        let header = snap.header.clone();
        let chunks = snap.bytes.clone();
        let video = std::thread::spawn(move || {
            let _ = stdin.write_all(&header);
            for c in &chunks {
                if stdin.write_all(c).is_err() {
                    break;
                }
            }
        });
        let pipe_name = pipe.as_ref().map(|p| p.name.clone());
        let audio_thread = pipe.map(|p| {
            let pcm = pcm.clone();
            std::thread::spawn(move || {
                if let Ok(mut w) = p.connect() {
                    let _ = w.write_all(&pcm);
                }
            })
        });
        let result = finish(child, &part, out);
        let _ = video.join();
        if let (Some(t), Some(name)) = (audio_thread, pipe_name) {
            if !t.is_finished() {
                Pipe::poke(&name);
            }
            let _ = t.join();
        }
        result
    };
    match attempt(false) {
        Ok(n) => Ok(n),
        // The reserved index space was too small (very long clip): classic way.
        Err(e) if e.contains("moov") || e.contains("reserved") => attempt(true),
        Err(e) => Err(e),
    }
}

/// A recording in progress: video and audio are streamed into ffmpeg live.
pub struct LiveMux {
    pub child: Child,
    pub part: PathBuf,
    pub out: PathBuf,
    pub stdin: Option<std::process::ChildStdin>,
    pub pipe: Option<Pipe>,
}

/// Starts a recording muxer writing a fragmented MP4 (crash-safe: everything
/// recorded so far stays playable even if the PC loses power).
pub fn start_live(ffmpeg: &Path, audio: &AudioOpts, out: &Path, register: &dyn Fn(&Child)) -> Result<LiveMux, String> {
    let part = part_path(out);
    let pipe = if audio.channels() > 0 { Some(Pipe::new()?) } else { None };
    let mut cmd = base_cmd(ffmpeg);
    cmd.args(["-f", "mpegts", "-i", "pipe:0"]);
    if let Some(p) = &pipe {
        audio_input(&mut cmd, &p.name, audio.channels());
    }
    cmd.args(audio_args(audio));
    cmd.args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"]).arg(&part);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    // Live recordings run at normal priority so they never fall behind.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    register(&child);
    let stdin = child.stdin.take();
    Ok(LiveMux { child, part, out: out.to_path_buf(), stdin, pipe })
}

impl LiveMux {
    pub fn finish(mut self) -> Result<u64, String> {
        drop(self.stdin.take());
        finish(self.child, &self.part, &self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(desktop: bool, mic: bool, separate: bool) -> AudioOpts {
        AudioOpts { desktop, mic, desktop_volume: 100, mic_volume: 150, noise_suppression: true, separate_tracks: separate }
    }

    #[test]
    fn audio_graphs() {
        let a = audio_args(&opts(true, true, true)).join(" ");
        assert!(a.contains("[1:a]asplit=2[x][y]"), "{a}");
        assert!(a.contains("pan=stereo|c0=c2|c1=c3,volume=1.50,afftdn=nf=-25[m]"), "{a}");
        assert!(a.contains("-map [mix] -metadata:s:a:0 title=Game + mic -map [d2] -metadata:s:a:1 title=Desktop -map [m2] -metadata:s:a:2 title=Microphone"), "{a}");
        let a = audio_args(&opts(true, true, false)).join(" ");
        assert!(a.contains("[d][m]amix") && !a.contains("[d2]"));
        let a = audio_args(&opts(true, false, true)).join(" ");
        assert!(a.contains("[1:a]volume=1.00[d]") && a.contains("-map [d]"));
        let a = audio_args(&opts(false, false, true)).join(" ");
        assert_eq!(a, "-map 0:v -c:v copy");
        assert_eq!(opts(true, true, true).channels(), 4);
    }

    #[test]
    fn interleaving() {
        let d = [1i16, 2, 3, 4];
        let m = [10i16, 20, 30, 40];
        let v = interleave(Some(&d), Some(&m));
        let s: Vec<i16> = v.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(s, vec![1, 2, 10, 20, 3, 4, 30, 40]);
        assert_eq!(interleave(Some(&d), None).len(), 8);
    }

    #[test]
    fn moov_reservation_is_generous() {
        // 1 min, 60 fps, 3 tracks: ~3600 + 8460 frames -> ~210 KB
        let s = moov_size(60.0, 60, 3);
        assert!(s > 200_000 && s < 300_000, "{s}");
    }

    #[test]
    fn part_names() {
        assert_eq!(part_path(Path::new(r"D:\a\b.mp4")), PathBuf::from(r"D:\a\b.mp4.part"));
    }
}
