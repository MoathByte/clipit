//! How much CPU and RAM Clipit uses: this process plus everything it started
//! (ffmpeg, the WebView2 page). Shown in the tray menu.

use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    /// Percent of the whole CPU (all cores = 100).
    pub cpu: f64,
    /// Private memory, bytes.
    pub ram: u64,
}

/// Remembers the previous reading, so CPU can be worked out between two.
#[derive(Default)]
pub struct Meter {
    last: Option<(Instant, u64)>,
}

impl Meter {
    pub fn read(&mut self) -> Usage {
        let (cpu_time, ram) = totals();
        let now = Instant::now();
        let cpu = match self.last {
            Some((t, before)) => {
                let wall = now.duration_since(t).as_secs_f64() * 1e7 * cores() as f64;
                if wall > 0.0 { (cpu_time.saturating_sub(before) as f64 / wall * 100.0).clamp(0.0, 100.0) } else { 0.0 }
            }
            None => 0.0,
        };
        self.last = Some((now, cpu_time));
        Usage { cpu, ram }
    }
}

pub fn label(u: Usage) -> String {
    let ram = u.ram as f64 / 1_048_576.0;
    let ram = if ram >= 1024.0 { format!("{:.1} GB", ram / 1024.0) } else { format!("{ram:.0} MB") };
    format!("CPU {:.0}% \u{00B7} RAM {ram}", u.cpu)
}

fn cores() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}

/// (CPU time in 100 ns, private bytes) of this process and all its descendants.
#[cfg(windows)]
fn totals() -> (u64, u64) {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let me = std::process::id();
    // parent → children, from one snapshot of every process
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    pairs.push((e.th32ProcessID, e.th32ParentProcessID));
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
    }
    let mut ours = vec![me];
    let mut i = 0;
    while i < ours.len() {
        let p = ours[i];
        for &(pid, parent) in &pairs {
            if parent == p && pid != p && !ours.contains(&pid) {
                ours.push(pid);
            }
        }
        i += 1;
    }
    let (mut cpu, mut ram) = (0u64, 0u64);
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    for pid in ours {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { continue };
            let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok() {
                cpu += ft(k) + ft(u);
            }
            let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
            if GetProcessMemoryInfo(h, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb).is_ok() {
                ram += m.PrivateUsage as u64;
            }
            let _ = CloseHandle(h);
        }
    }
    (cpu, ram)
}

#[cfg(not(windows))]
fn totals() -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process() {
        let mut m = Meter::default();
        m.read();
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        assert!(x > 0);
        let u = m.read();
        assert!(u.ram > 1_000_000, "{u:?}");
        assert!(label(u).starts_with("CPU "));
    }
}
