//! Tiny local HTTP server for clips, thumbnails and game art.
//!
//! Replaces Tauri's asset protocol, which (a) kept every played clip in WebView2
//! memory — over 1 GB after a few clips — and (b) failed most concurrent image
//! requests, leaving thumbnails blank. This server streams straight from disk
//! with HTTP Range support and `no-store`.
//!
//! It listens on 127.0.0.1 only and every request must carry a random token,
//! and the caller decides which files may be served.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

pub struct Streamer {
    pub base: String,
}

fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Parses "bytes=START-END" / "bytes=START-" / "bytes=-SUFFIX" into an inclusive range.
pub fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (a, b) = first.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let suffix: u64 = b.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (len.saturating_sub(suffix), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { len - 1 } else { b.parse::<u64>().ok()?.min(len - 1) };
        (start, end)
    };
    if start > end || start >= len {
        return None;
    }
    Some((start, end))
}

/// True if `path` is `dir` followed by exactly `depth` plain components
/// (no `..`, no drive or root tricks) — e.g. depth 1 = a file directly in `dir`.
pub fn is_plain_file_in(path: &std::path::Path, dir: &std::path::Path, depth: usize) -> bool {
    use std::path::Component;
    let Ok(rest) = path.strip_prefix(dir) else { return false };
    let parts: Vec<_> = rest.components().collect();
    parts.len() == depth && parts.iter().all(|c| matches!(c, Component::Normal(_)))
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

/// Starts the server. `allow` gets the requested path and returns the file to serve, or None.
pub fn start(allow: Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>) -> Result<Streamer, String> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or("no port")?;
    let token = random_token();
    let base = format!("http://127.0.0.1:{port}/v?t={token}&p=");
    let server = Arc::new(server);

    std::thread::Builder::new()
        .name("clip-stream".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                let allow = allow.clone();
                let token = token.clone();
                std::thread::spawn(move || handle(request, &token, allow.as_ref()));
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(Streamer { base })
}

fn handle(request: tiny_http::Request, token: &str, allow: &(dyn Fn(&str) -> Option<PathBuf> + Send + Sync)) {
    let respond_status = |req: tiny_http::Request, code: u16| {
        let _ = req.respond(tiny_http::Response::empty(code));
    };

    let url = match reqwest::Url::parse(&format!("http://local{}", request.url())) {
        Ok(u) => u,
        Err(_) => return respond_status(request, 400),
    };
    let mut t = None;
    let mut p = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "t" => t = Some(v.into_owned()),
            "p" => p = Some(v.into_owned()),
            _ => {}
        }
    }
    if url.path() != "/v" || t.as_deref() != Some(token) {
        return respond_status(request, 403);
    }
    #[cfg(debug_assertions)]
    let requested = p.clone().unwrap_or_default();
    let Some(path) = p.and_then(|p| allow(&p)) else {
        #[cfg(debug_assertions)]
        println!("[stream] 404 not allowed: {requested:?}");
        return respond_status(request, 404);
    };
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(_e) => {
            #[cfg(debug_assertions)]
            println!("[stream] 404 open failed: {path:?}: {_e}");
            return respond_status(request, 404);
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    let content_type = match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("mp4") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    };
    let mut headers = vec![
        header("Content-Type", content_type),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", "no-store"),
        header("Access-Control-Allow-Origin", "*"),
    ];

    let (status, start, count) = match range {
        Some(r) => match parse_range(&r, len) {
            Some((start, end)) => {
                headers.push(header("Content-Range", &format!("bytes {start}-{end}/{len}")));
                (206, start, end - start + 1)
            }
            None => {
                let mut resp = tiny_http::Response::empty(416);
                resp.add_header(header("Content-Range", &format!("bytes */{len}")));
                let _ = request.respond(resp);
                return;
            }
        },
        None => (200, 0, len),
    };

    if file.seek(SeekFrom::Start(start)).is_err() {
        return respond_status(request, 500);
    }
    let body = file.take(count);
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(status),
        headers,
        body,
        Some(count as usize),
        None,
    );
    // Errors here just mean the player stopped reading (seek / close) — that's normal.
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-500", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-2", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn plain_file_paths() {
        let dir = std::path::Path::new(r"C:\Users\X\AppData\Local\app\thumbs");
        assert!(is_plain_file_in(&dir.join("abc.jpg"), dir, 1));
        assert!(!is_plain_file_in(&dir.join(r"sub\abc.jpg"), dir, 1));
        assert!(!is_plain_file_in(&dir.join(r"..\secret.jpg"), dir, 1));
        assert!(!is_plain_file_in(std::path::Path::new(r"C:\Windows\x.jpg"), dir, 1));
        let art = std::path::Path::new(r"C:\Users\X\AppData\Local\app\art");
        assert!(is_plain_file_in(&art.join(r"0123abcd\grid.png"), art, 2));
        assert!(!is_plain_file_in(&art.join(r"..\..\grid.png"), art, 2));
    }

    #[test]
    fn serves_ranges_and_rejects_bad_tokens() {
        let dir = std::env::temp_dir().join(format!("clipit-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.mp4");
        std::fs::write(&file, (0u8..=255).collect::<Vec<_>>()).unwrap();
        let allowed = file.clone();
        let s = start(Arc::new(move |p: &str| {
            (PathBuf::from(p) == allowed).then(|| allowed.clone())
        }))
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let c = reqwest::Client::new();
            let url = format!("{}{}", s.base, urlencode(&file.to_string_lossy()));
            let r = c.get(&url).header("Range", "bytes=10-19").send().await.unwrap();
            assert_eq!(r.status().as_u16(), 206);
            assert_eq!(r.headers()["content-range"], "bytes 10-19/256");
            assert_eq!(r.headers()["cache-control"], "no-store");
            assert_eq!(r.bytes().await.unwrap().to_vec(), (10u8..20).collect::<Vec<_>>());

            let full = c.get(&url).send().await.unwrap();
            assert_eq!(full.status().as_u16(), 200);
            assert_eq!(full.bytes().await.unwrap().len(), 256);

            let bad = url.replace("t=", "t=x");
            assert_eq!(c.get(&bad).send().await.unwrap().status().as_u16(), 403);
            let other = format!("{}{}", s.base, urlencode("C:\\Windows\\win.ini"));
            assert_eq!(c.get(&other).send().await.unwrap().status().as_u16(), 404);
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn urlencode(s: &str) -> String {
        let mut u = reqwest::Url::parse("http://x/").unwrap();
        u.query_pairs_mut().append_pair("p", s);
        u.query().unwrap()[2..].to_string()
    }
}
