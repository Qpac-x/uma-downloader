//! HTTP fetch (with retry/backoff), manifest loading, and the resumable
//! per-asset download. Progress reporting is decoupled behind [`Progress`] so
//! the same download loop drives both the CLI (indicatif) and the GUI (egui).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::{StreamExt, TryStreamExt};
use reqwest::header::{CONTENT_TYPE, RANGE, USER_AGENT};
use reqwest::{Client, StatusCode};
use tokio::io::AsyncWriteExt;

use crate::manifest::{self, asset_filename, asset_url, Asset};

pub const USER_AGENT_VALUE: &str = "Mozilla/5.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Skip,
    Fail,
    Cancelled,
}

/// Progress sink for a download run. The download loop reports byte counts and
/// polls pause/cancel through this trait; the caller renders however it likes.
pub trait Progress: Send + Sync {
    /// Report `n` more bytes written to disk (aggregate progress).
    fn on_bytes(&self, _n: u64) {}

    /// True while the user has paused the run; the download waits until false.
    fn paused(&self) -> bool {
        false
    }

    /// True when the user has cancelled; the download aborts.
    fn cancelled(&self) -> bool {
        false
    }
}

fn backoff(attempt: u32) -> Duration {
    // Retries resume via Range, so a long backoff only wastes time; keep it
    // short (max 3s) so a transient mid-file reset recovers almost immediately.
    Duration::from_secs(2u64.pow(attempt).min(3))
}

/// GET a URL, retrying transient errors (timeouts, 5xx) with backoff.
/// Raises immediately on permanent errors (HTTP 4xx).
pub async fn http_get(client: &Client, url: &str, timeout_secs: u64) -> Result<Vec<u8>> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..6u32 {
        let result = client
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await;
        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_client_error() {
                    return Err(anyhow!("HTTP {status} for {url}"));
                }
                if !status.is_success() {
                    last = Some(anyhow!("HTTP {status} for {url}"));
                } else {
                    match resp.bytes().await {
                        Ok(b) => return Ok(b.to_vec()),
                        Err(e) => last = Some(anyhow!("read failed for {url}: {e}")),
                    }
                }
            }
            Err(e) => last = Some(anyhow!("request failed for {url}: {e}")),
        }
        if attempt < 5 {
            tokio::time::sleep(backoff(attempt)).await;
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("GET failed: {url}")))
}

/// LZ4-frame-decompress a manifest. CPU-bound: call from a blocking pool.
fn lz4_decompress(raw: Vec<u8>) -> Result<Vec<u8>> {
    let mut dec = lz4_flex::frame::FrameDecoder::new(&raw[..]);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut out).context("LZ4 decompress failed")?;
    Ok(out)
}

/// Fetch and LZ4-frame-decompress a manifest.
pub async fn fetch_manifest(client: &Client, url: &str) -> Result<Vec<u8>> {
    let raw = http_get(client, url, 60).await?;
    // Decompression is CPU-bound; run it on the blocking pool so it can't
    // stall the async runtime while category manifests are fetched in parallel.
    tokio::task::spawn_blocking(move || lz4_decompress(raw))
        .await
        .context("LZ4 decompress task panicked")?
}

/// Root manifests are tiny; category manifests are megabytes.
const MANIFEST_SCAN_MAX_BYTES: u64 = 2048;

/// Find the newest file in `dat/` that parses as a root manifest. The game
/// rewrites this at login, so it is the tree the game itself considers current;
/// only the root URL carries a resource version.
///
/// Returns `(filename, mtime, decompressed bytes)`.
fn find_local_root(dat_dir: &Path) -> Option<(String, std::time::SystemTime, Vec<u8>)> {
    let mut best: Option<(std::time::SystemTime, String, Vec<u8>)> = None;
    for sub in std::fs::read_dir(dat_dir.join("dat")).ok()?.flatten() {
        let sub_path = sub.path();
        if !sub_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&sub_path) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() || md.len() > MANIFEST_SCAN_MAX_BYTES {
                continue;
            }
            let Ok(raw) = std::fs::read(e.path()) else {
                continue;
            };
            if !raw.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
                continue;
            }
            let Ok(dec) = lz4_decompress(raw) else {
                continue;
            };
            let Ok(parsed) = manifest::parse_platform(&dec) else {
                continue;
            };
            if !parsed.iter().any(|p| p.name == b"Windows") {
                continue;
            }
            let mtime = md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().map_or(true, |(t, _, _)| mtime > *t) {
                best = Some((mtime, e.file_name().to_string_lossy().into_owned(), dec));
            }
        }
    }
    best.map(|(t, name, d)| (name, t, d))
}

/// Read a manifest by content hash: prefer the game's cached copy, otherwise
/// fetch it (manifest URLs under /resources/ are version-independent).
async fn read_manifest(
    client: Client,
    dat_dir: PathBuf,
    name: Vec<u8>,
    size: u64,
    checksum: [u8; 8],
) -> Result<Vec<u8>> {
    let fn_ = manifest::manifest_filename(&name, size, &checksum);
    let local = dat_dir.join("dat").join(&fn_[..2]).join(&fn_);
    if let Ok(md) = tokio::fs::metadata(&local).await {
        if md.len() == size {
            let raw = tokio::fs::read(&local).await?;
            return tokio::task::spawn_blocking(move || lz4_decompress(raw))
                .await
                .context("LZ4 decompress task panicked")?;
        }
    }
    fetch_manifest(&client, &manifest::manifest_url(&name, size, &checksum)).await
}

/// Walk the manifest tree (root -> Windows platform -> all category manifests)
/// and flatten every asset. The tree is read from the game's own cache in
/// `dat/` (refreshed by the game at login), falling back to the CDN by content
/// hash. Category manifests are fetched concurrently.
///
/// Takes `client` by value (cheap `Arc` clone) so each spawned future owns its
/// client instead of borrowing it across the `buffer_unordered` concurrency
/// boundary — this is what makes the returned future `Send` without the
/// async-closure HRTB limitation.
pub struct ManifestSet {
    pub assets: Vec<Asset>,
    /// Filename of the root manifest the tree came from. The game stores no
    /// resource version, so this is the closest thing to a data version.
    pub root_name: String,
    pub root_mtime: Option<std::time::SystemTime>,
}

pub async fn load_all_entries(
    client: Client,
    dat_dir: String,
    resource_version: Option<String>,
) -> Result<ManifestSet> {
    use manifest::*;

    let (root, root_name, root_mtime) = match resource_version {
        Some(v) => {
            let url = format!("{CDN}/{v}/manifests/manifestdat/root.manifest.bsv.lz4");
            let root = fetch_manifest(&client, &url)
                .await
                .with_context(|| format!("fetch root manifest for version {v}"))?;
            (root, format!("resource {v}"), None)
        }
        None => {
            let dir = PathBuf::from(&dat_dir);
            let local = tokio::task::spawn_blocking(move || find_local_root(&dir))
                .await
                .context("manifest scan task panicked")?;
            match local {
                Some((name, mtime, root)) => {
                    eprintln!("manifests: using cached root {name} from dat/");
                    (root, name, Some(mtime))
                }
                None => anyhow::bail!(
                    "no cached root manifest in {dat_dir}/dat; launch the game once so it syncs manifests, or pass --resource-version <v>"
                ),
            }
        }
    };
    let platforms = parse_platform(&root)?;
    let win = platforms
        .iter()
        .find(|p| p.name == b"Windows")
        .ok_or_else(|| anyhow!("no Windows platform in root manifest"))?;
    let plat = read_manifest(
        client.clone(),
        PathBuf::from(&dat_dir),
        win.name.clone(),
        win.size,
        win.checksum,
    )
    .await?;
    let categories = parse_platform(&plat)?;

    // Fetch the ~37 category manifests concurrently (bounded). Futures are
    // built in a plain loop over owned data (no closure over a reference
    // parameter), which sidesteps the async-closure HRTB / Send limitation.
    let mut futs = Vec::with_capacity(categories.len());
    for p in &categories {
        let client = client.clone();
        let dat_dir = PathBuf::from(&dat_dir);
        let name = p.name.clone();
        let size = p.size;
        let checksum = p.checksum;
        futs.push(async move {
            let dec = read_manifest(client, dat_dir, name.clone(), size, checksum).await?;
            let entries = parse_category(&dec)?;
            Ok::<_, anyhow::Error>((name, entries))
        });
    }
    let parsed: Vec<(Vec<u8>, Vec<CategoryEntry>)> = futures::stream::iter(futs)
        .buffer_unordered(8)
        .try_collect()
        .await?;

    let mut assets = Vec::new();
    for (cat, entries) in parsed {
        for e in entries {
            // names[0] is the downloadable path (name2 is a dependency list).
            assets.push(Asset {
                category: cat.clone(),
                name: e.names[0].clone(),
                size: e.size,
                h: e.h,
            });
        }
    }
    Ok(ManifestSet {
        assets,
        root_name,
        root_mtime,
    })
}

fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// One planned download: everything a worker needs.
pub struct Task {
    pub url: String,
    pub dest: PathBuf,
    pub size: u64,
    pub display: String,
}

/// Build the download list for the assets `keep` accepts, skipping files already
/// complete on disk. Synchronous (one SHA1 + metadata per asset): call from
/// `spawn_blocking` when the asset list is large.
pub fn plan_tasks(
    assets: &[Asset],
    dat_dir: &str,
    keep: impl Fn(&Asset) -> bool,
) -> (Vec<Task>, usize) {
    let mut todo = Vec::new();
    let mut skipped = 0usize;
    for a in assets {
        if !keep(a) {
            continue;
        }
        let fn_ = asset_filename(&a.name, a.size, &a.h);
        let url = asset_url(&a.category, &fn_);
        let dest = PathBuf::from(dat_dir)
            .join("dat")
            .join(&fn_[..2])
            .join(&fn_);
        match std::fs::metadata(&dest) {
            Ok(md) if md.len() == a.size => skipped += 1,
            _ => todo.push(Task {
                url,
                dest,
                size: a.size,
                display: String::from_utf8_lossy(&a.name).into_owned(),
            }),
        }
    }
    (todo, skipped)
}

/// Download a single asset, resumable. Returns the outcome; never panics on
/// network or filesystem errors. Honors pause/cancel via `progress`.
pub async fn download_one(
    client: &Client,
    url: &str,
    dest: &Path,
    size: u64,
    progress: &dyn Progress,
) -> Outcome {
    // Honor pause/cancel before touching the filesystem or network.
    while progress.paused() && !progress.cancelled() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if progress.cancelled() {
        return Outcome::Cancelled;
    }

    match tokio::fs::metadata(dest).await {
        Ok(md) if md.len() == size => {
            progress.on_bytes(size); // counts toward the caller's full-size total
            return Outcome::Skip;
        }
        Ok(_) => {
            let _ = tokio::fs::remove_file(dest).await; // corrupt/partial from a previous scheme
        }
        Err(_) => {}
    }

    if let Some(dir) = dest.parent() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            eprintln!("  ! cannot create {}: {e}", dir.display());
            eprintln!("    run elevated or pass --dat-dir to a writable location");
            return Outcome::Fail;
        }
    }

    let part = part_path(dest);
    for attempt in 0..6u32 {
        if progress.cancelled() {
            return Outcome::Cancelled;
        }
        // A previous run may have left a complete .part file.
        if let Ok(md) = tokio::fs::metadata(&part).await {
            if md.len() == size {
                if tokio::fs::rename(&part, dest).await.is_ok() {
                    if attempt == 0 {
                        progress.on_bytes(size);
                    }
                    return Outcome::Ok;
                }
            }
        }

        let mut have = match tokio::fs::metadata(&part).await {
            Ok(md) if md.len() <= size => md.len(),
            Ok(_) => 0, // oversized partial; restart
            Err(_) => 0,
        };
        // Resumed bytes are already on disk: report them once so the bar can reach
        // its full-size total. shortcut: a restart after an ignored Range re-reports
        // these bytes; accept the overshoot vs. tracking per-file disk state.
        if attempt == 0 && have > 0 {
            progress.on_bytes(have);
        }

        let mut req = client.get(url).header(USER_AGENT, USER_AGENT_VALUE);
        if have > 0 {
            req = req.header(RANGE, format!("bytes={have}-"));
        }
        let resp = tokio::select! {
            r = req.send() => match r {
                Ok(r) => r,
                Err(_) => {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            },
            _ = wait_cancelled(progress) => return Outcome::Cancelled,
            // Headers can stall too; without this a silent server pins the worker forever.
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        let status = resp.status();
        if status.is_client_error() {
            return Outcome::Fail; // 4xx is permanent
        }
        if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
            tokio::time::sleep(backoff(attempt)).await; // 5xx etc -> transient
            continue;
        }
        if have > 0 && status == StatusCode::OK {
            have = 0; // server ignored Range; start over
        }
        if have == 0 {
            let ct = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if ct.contains("text/html") {
                return Outcome::Fail; // HTML error page, not an asset
            }
        }
        match stream_to_file(resp, &part, have, progress).await {
            Ok(()) => {}
            Err(_) if progress.cancelled() => return Outcome::Cancelled,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) =>
            {
                eprintln!("  ! permission denied writing to {}", dest.display());
                eprintln!("    run elevated or pass --dat-dir to a writable location");
                return Outcome::Fail;
            }
            Err(_e) => {
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        }
        if let Ok(md) = tokio::fs::metadata(&part).await {
            if md.len() == size {
                if tokio::fs::rename(&part, dest).await.is_ok() {
                    return Outcome::Ok;
                }
            }
        }
    }
    if progress.cancelled() {
        Outcome::Cancelled
    } else {
        Outcome::Fail
    }
}

async fn stream_to_file(
    resp: reqwest::Response,
    part: &Path,
    have: u64,
    progress: &dyn Progress,
) -> Result<()> {
    let mut file = if have > 0 {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .await?
    } else {
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(part)
            .await?
    };
    let mut stream = resp.bytes_stream();
    loop {
        // Honor pause before pulling more data.
        while progress.paused() && !progress.cancelled() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if progress.cancelled() {
            return Err(anyhow!("cancelled"));
        }
        // Wait for the next chunk, but stay responsive to cancel even if the
        // connection stalls (no chunk arrives), and time out a genuinely
        // stalled read so the outer retry loop restarts it instead of hanging.
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = wait_cancelled(progress) => return Err(anyhow!("cancelled")),
            _ = tokio::time::sleep(Duration::from_secs(15)) => return Err(anyhow!("read stalled")),
        };
        let chunk = match chunk {
            Some(c) => c?,
            None => break,
        };
        let n = chunk.len() as u64;
        file.write_all(&chunk).await?;
        progress.on_bytes(n);
    }
    file.flush().await?;
    Ok(())
}

/// Resolves as soon as `progress.cancelled()` is true (polled every 100ms).
async fn wait_cancelled(progress: &dyn Progress) {
    while !progress.cancelled() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal platform-manifest bytes (header + one Windows entry).
    fn encode_root(size_byte: u8) -> Vec<u8> {
        let mut m = vec![0u8, 0, 0, 1, 1];
        m.extend_from_slice(&[0u8; 7]);
        m.extend_from_slice(b"Windows\0");
        m.push(size_byte);
        m.extend_from_slice(&[0u8; 8]);
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        std::io::Write::write_all(&mut enc, &m).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn find_local_root_picks_newest_manifest() {
        let base = std::env::temp_dir().join(format!("uma-root-test-{}", std::process::id()));
        let a = base.join("dat").join("AA");
        let b = base.join("dat").join("BB");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        std::fs::write(a.join("junk"), b"not a manifest").unwrap();
        std::fs::write(a.join("old"), encode_root(10)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(b.join("new"), encode_root(20)).unwrap();

        let (name, _, got) = find_local_root(&base).expect("root manifest");
        assert_eq!(name, "new");
        let entries = manifest::parse_platform(&got).unwrap();
        assert_eq!(entries[0].size, 20);

        std::fs::remove_dir_all(&base).unwrap();
    }
}
