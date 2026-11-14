//! A/V sync test signal: a small window that flashes white while a 1 kHz beep
//! plays, once per second. Record it with Clipit, then measure the offset.
//!
//! cargo run --example avsync -- <output device name part> <x> <y> [count]
//! The window never takes focus, so it can run next to a game.

use std::time::{Duration, Instant};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static mut WHITE: bool = false;

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(h, &mut ps);
        let brush = CreateSolidBrush(if WHITE { COLORREF(0xffffff) } else { COLORREF(0) });
        FillRect(dc, &ps.rcPaint, brush);
        let _ = DeleteObject(brush.into());
        let _ = EndPaint(h, &ps);
        return LRESULT(0);
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dev_part = args.get(1).cloned().unwrap_or_default();
    let x: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let y: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100);
    let count: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);

    // Audio: keep a stream running (silence), write the beep at flash time.
    let _ = wasapi::initialize_mta();
    let en = DeviceEnumerator::new().unwrap();
    let coll = en.get_device_collection(Direction::Render).unwrap();
    let dev = (&coll)
        .into_iter()
        .flatten()
        .find(|d| d.get_friendlyname().map(|n| n.contains(&dev_part)).unwrap_or(false))
        .expect("output device not found");
    println!("audio device: {}", dev.get_friendlyname().unwrap());
    let mut client = dev.get_iaudioclient().unwrap();
    let fmt = WaveFormat::new(32, 32, SampleType::Float, 48000, 2, None);
    let (_, min_period) = client.get_device_period().unwrap();
    client
        .initialize_client(&fmt, Direction::Render, StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: min_period.max(100_000) }, None)
        .unwrap();
    let render = client.get_audiorenderclient().unwrap();
    client.start_stream().unwrap();

    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        let class = w!("ClipitAvSync");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class,
            w!("avsync"),
            WS_POPUP | WS_VISIBLE,
            x, y, 400, 400,
            None, None, Some(inst.into()), None,
        )
        .unwrap();
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);

        let start = Instant::now();
        let mut beep_left = 0usize; // frames of tone still to write
        let mut phase = 0f32;
        let mut next_flash = 1u32;
        let mut flash_off_at: Option<Instant> = None;
        while next_flash <= count + 1 {
            // Pump window messages.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let t = start.elapsed();
            if next_flash <= count && t >= Duration::from_secs(next_flash as u64) {
                WHITE = true;
                let _ = InvalidateRect(Some(h), None, true);
                let _ = UpdateWindow(h);
                beep_left = 2400; // 50 ms
                flash_off_at = Some(Instant::now() + Duration::from_millis(100));
                next_flash += 1;
            } else if next_flash > count && t >= Duration::from_secs(next_flash as u64) {
                break;
            }
            if let Some(off) = flash_off_at {
                if Instant::now() >= off {
                    WHITE = false;
                    let _ = InvalidateRect(Some(h), None, true);
                    let _ = UpdateWindow(h);
                    flash_off_at = None;
                }
            }
            // Keep the render buffer topped up.
            let space = client.get_available_space_in_frames().unwrap() as usize;
            if space > 0 {
                let mut data = Vec::with_capacity(space * 8);
                for _ in 0..space {
                    let v = if beep_left > 0 {
                        beep_left -= 1;
                        phase += 2.0 * std::f32::consts::PI * 1000.0 / 48000.0;
                        phase.sin() * 0.8
                    } else {
                        0.0
                    };
                    data.extend_from_slice(&v.to_le_bytes());
                    data.extend_from_slice(&v.to_le_bytes());
                }
                render.write_to_device(&data, None).unwrap();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = DestroyWindow(h);
    }
    client.stop_stream().unwrap();
    println!("done");
}
