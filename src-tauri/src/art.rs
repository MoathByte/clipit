//! Game artwork from SteamGridDB (https://www.steamgriddb.com), cached on disk.

use crate::media::fnv1a;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const API: &str = "https://www.steamgriddb.com/api/v2";

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct GameArt {
    /// Tall cover (600x900).
    pub grid: Option<String>,
    /// Wide banner shown at the top of a game page.
    pub hero: Option<String>,
    /// Transparent game logo.
    pub logo: Option<String>,
    /// Small square icon for the sidebar.
    pub icon: Option<String>,
    /// SteamGridDB's name for the game, if found.
    pub matched_name: Option<String>,
}

#[derive(Deserialize)]
struct ApiList<T> {
    success: bool,
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Deserialize)]
struct SearchHit {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct Asset {
    url: String,
}

fn game_dir(cache: &Path, name: &str) -> PathBuf {
    cache.join(format!("{:016x}", fnv1a(name.to_lowercase().as_bytes())))
}

pub fn cached(cache: &Path, name: &str) -> Option<GameArt> {
    let meta = game_dir(cache, name).join("meta.json");
    let text = std::fs::read_to_string(meta).ok()?;
    serde_json::from_str(&text).ok()
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    key: &str,
    url: reqwest::Url,
) -> Result<ApiList<T>, String> {
    let resp = http
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("SteamGridDB rejected the API key".into());
    }
    if !resp.status().is_success() {
        return Err(format!("SteamGridDB returned {}", resp.status()));
    }
    resp.json::<ApiList<T>>().await.map_err(|e| e.to_string())
}

async fn first_asset(
    http: &reqwest::Client,
    key: &str,
    kind: &str,
    id: u64,
    query: &[(&str, &str)],
) -> Option<String> {
    let mut url = reqwest::Url::parse(&format!("{API}/{kind}/game/{id}")).ok()?;
    for (k, v) in query {
        url.query_pairs_mut().append_pair(k, v);
    }
    let list: ApiList<Asset> = get_json(http, key, url).await.ok()?;
    if !list.success {
        return None;
    }
    list.data.into_iter().next().map(|a| a.url)
}

async fn download(http: &reqwest::Client, url: &str, dest_stem: &Path) -> Option<String> {
    let ext = url
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("png");
    let dest = dest_stem.with_extension(ext);
    let bytes = http.get(url).send().await.ok()?.bytes().await.ok()?;
    std::fs::write(&dest, &bytes).ok()?;
    Some(dest.to_string_lossy().into_owned())
}

pub async fn fetch(
    http: &reqwest::Client,
    key: &str,
    cache: &Path,
    name: &str,
) -> Result<GameArt, String> {
    let mut search = reqwest::Url::parse(&format!("{API}/search/autocomplete/")).unwrap();
    search
        .path_segments_mut()
        .map_err(|_| "bad url")?
        .pop_if_empty()
        .push(name);
    let hits: ApiList<SearchHit> = get_json(http, key, search).await?;

    let dir = game_dir(cache, name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut art = GameArt::default();
    if let Some(hit) = hits.data.into_iter().next() {
        art.matched_name = Some(hit.name);
        let (grid, hero, logo, icon) = tokio::join!(
            first_asset(http, key, "grids", hit.id, &[("dimensions", "600x900")]),
            first_asset(http, key, "heroes", hit.id, &[]),
            first_asset(http, key, "logos", hit.id, &[]),
            first_asset(http, key, "icons", hit.id, &[]),
        );
        if let Some(u) = grid {
            art.grid = download(http, &u, &dir.join("grid")).await;
        }
        if let Some(u) = hero {
            art.hero = download(http, &u, &dir.join("hero")).await;
        }
        if let Some(u) = logo {
            art.logo = download(http, &u, &dir.join("logo")).await;
        }
        if let Some(u) = icon {
            art.icon = download(http, &u, &dir.join("icon")).await;
        }
    }

    // Cache even "not found" results so we don't hit the API every launch.
    let json = serde_json::to_string_pretty(&art).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("meta.json"), json).map_err(|e| e.to_string())?;
    Ok(art)
}

pub fn clear(cache: &Path) {
    let _ = std::fs::remove_dir_all(cache);
}
