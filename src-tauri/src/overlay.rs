//! Overlay helpers (Windows): a downscaled screenshot of the screen behind
//! Clipit (the UI blurs it in, like a theatre-mode backdrop), and a check for
//! whether the user has clicked away to another app.

use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Output size for a capture: at most `max_w` wide, same aspect ratio.
pub fn scaled_size(w: u32, h: u32, max_w: u32) -> (u32, u32) {
    if w <= max_w || w == 0 {
        return (w, h);
    }
    (max_w, ((h as u64 * max_w as u64 + w as u64 / 2) / w as u64).max(1) as u32)
}

/// A 24-bit bottom-up BMP file from rows GDI already gave us in BMP order.
pub fn bmp_file(w: u32, h: u32, rows: &[u8]) -> Vec<u8> {
    let stride = ((w * 3 + 3) & !3) as usize;
    debug_assert_eq!(rows.len(), stride * h as usize);
    let data_len = rows.len() as u32;
    let mut out = Vec::with_capacity(54 + rows.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + data_len).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(rows);
    out
}

/// True if the picture is essentially black (some fullscreen games can't be
/// captured this way and come back black — then the UI uses a plain dark tint).
pub fn is_black(rows: &[u8]) -> bool {
    if rows.is_empty() {
        return true;
    }
    let step = (rows.len() / 4096).max(1);
    let (mut sum, mut n) = (0u64, 0u64);
    for b in rows.iter().step_by(step) {
        sum += *b as u64;
        n += 1;
    }
    sum / n.max(1) < 3
}

/// A heavily blurred, small copy of a capture (24-bit bottom-up rows). The page
/// stretches it over the screen and simply fades it in — far cheaper than a live
/// CSS blur, so the entrance never stutters. `strength` 0–100.
pub fn blurred(w: u32, h: u32, rows: &[u8], strength: u32) -> (u32, u32, Vec<u8>) {
    let stride = ((w * 3 + 3) & !3) as usize;
    let bw = w.min(320).max(1);
    let bh = ((h as u64 * bw as u64 / w.max(1) as u64) as u32).max(1);
    // Downscale by averaging the source pixels that fall into each target pixel.
    let mut img = vec![0f32; (bw * bh * 3) as usize];
    for y in 0..bh {
        let (y0, y1) = (y * h / bh, ((y + 1) * h / bh).max(y * h / bh + 1).min(h));
        for x in 0..bw {
            let (x0, x1) = (x * w / bw, ((x + 1) * w / bw).max(x * w / bw + 1).min(w));
            let mut acc = [0u32; 3];
            for sy in y0..y1 {
                let row = &rows[sy as usize * stride..];
                for sx in x0..x1 {
                    for c in 0..3 {
                        acc[c] += row[sx as usize * 3 + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)).max(1) as f32;
            for c in 0..3 {
                img[((y * bw + x) * 3) as usize + c] = acc[c] as f32 / n;
            }
        }
    }
    // Three box-blur passes ≈ a gaussian. Radius in small-image pixels.
    let radius = (2 + strength.min(100) * 10 / 100) as i32;
    for _ in 0..3 {
        img = box_pass(&img, bw as i32, bh as i32, radius, true);
        img = box_pass(&img, bw as i32, bh as i32, radius, false);
    }
    let ostride = ((bw * 3 + 3) & !3) as usize;
    let mut out = vec![0u8; ostride * bh as usize];
    for y in 0..bh as usize {
        for x in 0..bw as usize {
            for c in 0..3 {
                out[y * ostride + x * 3 + c] = img[(y * bw as usize + x) * 3 + c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    (bw, bh, out)
}

fn box_pass(src: &[f32], w: i32, h: i32, r: i32, horizontal: bool) -> Vec<f32> {
    let mut dst = vec![0f32; src.len()];
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: i32, i: i32| -> usize {
        let i = i.clamp(0, len - 1);
        (if horizontal { line * w + i } else { i * w + line }) as usize * 3
    };
    let n = (2 * r + 1) as f32;
    for line in 0..lines {
        for c in 0..3 {
            let mut sum = 0f32;
            for i in -r..=r {
                sum += src[idx(line, i) + c];
            }
            for i in 0..len {
                dst[idx(line, i) + c] = sum / n;
                sum += src[idx(line, i + r + 1) + c] - src[idx(line, i - r) + c];
            }
        }
    }
    dst
}

/// Captures `r` (physical pixels) scaled to at most `max_w` wide and writes it to
/// `out` as a BMP, plus a blurred copy to `blur.0` if asked. Takes ~10–30 ms.
#[cfg(windows)]
pub fn capture(r: Rect, max_w: u32, out: &Path, blur: Option<(&Path, u32)>) -> Result<(), String> {
    use windows::Win32::Graphics::Gdi::*;
    let (ow, oh) = scaled_size(r.w, r.h, max_w);
    if ow == 0 || oh == 0 {
        return Err("empty screen".into());
    }
    let stride = ((ow * 3 + 3) & !3) as usize;
    let mut rows = vec![0u8; stride * oh as usize];
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err("no screen DC".into());
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, ow as i32, oh as i32);
        let old = SelectObject(mem, bmp.into());
        SetStretchBltMode(mem, HALFTONE);
        let _ = SetBrushOrgEx(mem, 0, 0, None);
        let blt = StretchBlt(
            mem, 0, 0, ow as i32, oh as i32,
            Some(screen), r.x, r.y, r.w as i32, r.h as i32,
            SRCCOPY,
        );
        SelectObject(mem, old);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ow as i32,
                biHeight: oh as i32, // bottom-up, the same row order as a BMP file
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            mem, bmp, 0, oh,
            Some(rows.as_mut_ptr() as *mut _),
            &mut info, DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !blt.as_bool() || lines != oh as i32 {
            return Err("screen capture failed".into());
        }
    }
    if is_black(&rows) {
        return Err("screen came back black".into());
    }
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, bmp_file(ow, oh, &rows)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, out).map_err(|e| e.to_string())?;
    if let Some((blur_out, strength)) = blur {
        let (bw, bh, b) = blurred(ow, oh, &rows, strength);
        let tmp = blur_out.with_extension("tmp");
        std::fs::write(&tmp, bmp_file(bw, bh, &b)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, blur_out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn capture(_r: Rect, _max_w: u32, _out: &Path, _blur: Option<(&Path, u32)>) -> Result<(), String> {
    Err("not supported".into())
}

/// Whether the active (foreground) window is Clipit or one of its dialogs.
/// None while Windows has no foreground window (e.g. mid-switch).
#[cfg(windows)]
pub fn foreground_is_ours(main_hwnd: isize) -> Option<bool> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        if fg.0 as isize == main_hwnd || GetAncestor(fg, GA_ROOTOWNER) == HWND(main_hwnd as *mut _) {
            return Some(true);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        Some(pid == std::process::id())
    }
}

#[cfg(not(windows))]
pub fn foreground_is_ours(_main_hwnd: isize) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(scaled_size(1920, 1080, 1280), (1280, 720));
        assert_eq!(scaled_size(2560, 1440, 1280), (1280, 720));
        assert_eq!(scaled_size(1280, 1024, 1280), (1280, 1024));
        assert_eq!(scaled_size(3440, 1440, 1280), (1280, 536));
    }

    #[test]
    fn bmp_header() {
        let (w, h) = (3u32, 2u32);
        let stride = 12usize; // 9 bytes padded to 12
        let f = bmp_file(w, h, &vec![7u8; stride * h as usize]);
        assert_eq!(&f[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(f[2..6].try_into().unwrap()) as usize, f.len());
        assert_eq!(u32::from_le_bytes(f[10..14].try_into().unwrap()), 54);
        assert_eq!(i32::from_le_bytes(f[18..22].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(f[22..26].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(f[28..30].try_into().unwrap()), 24);
        assert_eq!(f.len(), 54 + 24);
    }

    #[test]
    fn blur_smooths_and_keeps_average() {
        // 64x32 image: left half black, right half white.
        let (w, h) = (64u32, 32u32);
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut rows = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 32..64 {
                for c in 0..3 {
                    rows[y * stride + x * 3 + c] = 255;
                }
            }
        }
        let (bw, bh, b) = blurred(w, h, &rows, 50);
        assert_eq!((bw, bh), (64, 32));
        let px = |x: usize| b[16 * (((bw * 3 + 3) & !3) as usize) + x * 3];
        assert!(px(0) < 10 && px(63) > 245, "far edges stay put: {} {}", px(0), px(63));
        assert!(px(31) > 60 && px(31) < 195, "the edge is soft: {}", px(31));
        assert!(px(28) < px(31) && px(31) < px(35));
        // Big captures get shrunk to 320 wide.
        let (bw, bh, _) = blurred(1280, 720, &vec![9u8; 1280 * 3 * 720], 60);
        assert_eq!((bw, bh), (320, 180));
    }

    #[test]
    fn black_detection() {
        assert!(is_black(&vec![0u8; 10_000]));
        assert!(is_black(&vec![1u8; 10_000]));
        assert!(!is_black(&vec![40u8; 10_000]));
        assert!(is_black(&[]));
    }

    /// Captures the real screen. Run with: cargo test capture_real -- --ignored
    #[test]
    #[ignore]
    fn capture_real() {
        let out = std::env::temp_dir().join("clipit-backdrop-test.bmp");
        let blur_out = std::env::temp_dir().join("clipit-backdrop-test-blur.bmp");
        let t = std::time::Instant::now();
        capture(Rect { x: 0, y: 0, w: 1920, h: 1080 }, 1280, &out, Some((&blur_out, 60))).unwrap();
        println!("capture + blur took {:?}", t.elapsed());
        let len = std::fs::metadata(&out).unwrap().len();
        assert_eq!(len, 54 + 1280 * 3 * 720);
        println!("wrote {}", out.display());
    }
}
