//! Persist Subsonic / Navidrome audio to disk so a track can play without the network.
//!
//! Inspired by Feishin's music-cache: an explicit "save offline" keeps the file, and playback
//! prefers the cached copy when it is present. Automatic play-through caching is left for later.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Track;
use crate::subsonic::auth;
use crate::subsonic::client::SubsonicClient;

const MANIFEST: &str = "manifest.json";
const AUDIO_EXT: &str = "audio";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedTrack {
    pub id: String,
    pub name: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub bytes: u64,
    pub saved_at: i64,
    /// Relative file name under the cache root (`{id}.audio`).
    pub file: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Account key (`Credentials::account_key`) this directory belongs to.
    server: String,
    tracks: HashMap<String, CachedTrack>,
}

fn base() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join("offline")
        .join("subsonic")
}

/// Stable directory name for an account: hash of [`Credentials::account_key`], so multi-URL
/// picks for the same account share one cache and a different account never overwrites it.
fn dir_name(account_key: &str) -> String {
    format!("{:x}", Sha256::digest(account_key.as_bytes()))
}

fn root_for(account_key: &str) -> PathBuf {
    base().join(dir_name(account_key))
}

fn root() -> Option<PathBuf> {
    Some(root_for(&server_key()?))
}

fn manifest_path_for(account_key: &str) -> PathBuf {
    root_for(account_key).join(MANIFEST)
}

fn file_for_in(dir: &Path, id: &str) -> PathBuf {
    // Hash ids that contain path separators so a server id cannot escape the cache dir.
    let safe = match id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        true => id.to_owned(),
        false => format!("{:x}", Sha256::digest(id.as_bytes())),
    };
    dir.join(format!("{safe}.{AUDIO_EXT}"))
}

fn file_for(id: &str) -> Option<PathBuf> {
    Some(file_for_in(&root()?, id))
}

struct Held {
    /// `account_key` the in-memory manifest belongs to. Empty when nothing is loaded.
    key: String,
    manifest: Manifest,
}

fn held() -> &'static Mutex<Held> {
    static HELD: OnceLock<Mutex<Held>> = OnceLock::new();
    HELD.get_or_init(|| {
        Mutex::new(Held {
            key: String::new(),
            manifest: Manifest {
                version: 1,
                ..Manifest::default()
            },
        })
    })
}

/// Reload the in-memory index for the signed-in account (or clear it after logout).
pub fn reload() {
    let Ok(mut guard) = held().lock() else {
        return;
    };
    match server_key() {
        Some(key) => {
            guard.manifest = load_manifest_for(&key);
            guard.key = key;
        }
        None => {
            guard.key.clear();
            guard.manifest = Manifest {
                version: 1,
                ..Manifest::default()
            };
        }
    }
}

fn ensure(guard: &mut Held) -> Option<String> {
    let key = server_key()?;
    if guard.key != key {
        guard.manifest = load_manifest_for(&key);
        guard.key = key.clone();
    }
    Some(key)
}

fn cancelled() -> &'static Mutex<HashSet<String>> {
    static CANCELLED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CANCELLED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_cancelled(track_id: &str) -> bool {
    cancelled()
        .lock()
        .map(|set| set.contains(track_id))
        .unwrap_or(false)
}

fn clear_cancelled(track_id: &str) {
    if let Ok(mut set) = cancelled().lock() {
        set.remove(track_id);
    }
}

/// Abort an in-flight save for `track_id`: mark it cancelled and drop any partial file.
/// The download task should also be aborted by the caller; this covers the race after the
/// bytes have arrived but before the final rename.
pub fn cancel(track_id: &str) {
    if let Ok(mut set) = cancelled().lock() {
        set.insert(track_id.to_owned());
    }
    if let Some(dest) = file_for(track_id) {
        let part = dest.with_extension("part");
        if part.exists() {
            let _ = fs::remove_file(&part);
        }
    }
}

fn load_manifest_for(account_key: &str) -> Manifest {
    let path = manifest_path_for(account_key);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Manifest {
            version: 1,
            server: account_key.to_owned(),
            ..Manifest::default()
        },
    }
}

fn persist(account_key: &str, manifest: &Manifest) -> Result<()> {
    let dir = root_for(account_key);
    fs::create_dir_all(&dir).context("cannot create the offline cache folder")?;
    let path = manifest_path_for(account_key);
    let tmp = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(manifest).context("cannot serialize the offline index")?;
    {
        let mut file = fs::File::create(&tmp).context("cannot write the offline index")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path).context("cannot replace the offline index")?;
    Ok(())
}

fn server_key() -> Option<String> {
    auth::load().map(|credentials| credentials.account_key())
}

fn client() -> Result<Option<SubsonicClient>> {
    let Some(mut remembered) = auth::load() else {
        return Ok(None);
    };
    if remembered.signature.is_none() {
        remembered.signature = Some(auth::sign(&remembered.username, &remembered.password));
    }
    let signature = remembered.signature.clone().unwrap_or_default();
    Ok(Some(SubsonicClient::new(
        remembered.server,
        remembered.username,
        remembered.password,
        &signature,
    )?))
}

/// Whether any finished offline copy is on disk for the signed-in server.
pub fn any_ready() -> bool {
    !list().is_empty()
}

/// Whether a finished offline copy of `track_id` is on disk for the signed-in server.
pub fn is_cached(track_id: &str) -> bool {
    path(track_id).is_some()
}

/// Absolute path of a ready offline file, if it exists and still matches the index.
pub fn path(track_id: &str) -> Option<PathBuf> {
    let Ok(mut guard) = held().lock() else {
        return None;
    };
    let key = ensure(&mut guard)?;
    let entry = guard.manifest.tracks.get(track_id)?;
    let path = root_for(&key).join(&entry.file);
    path.is_file().then_some(path)
}

pub fn list() -> Vec<CachedTrack> {
    let Ok(mut guard) = held().lock() else {
        return Vec::new();
    };
    if ensure(&mut guard).is_none() {
        return Vec::new();
    }
    let mut tracks: Vec<_> = guard.manifest.tracks.values().cloned().collect();
    tracks.sort_by_key(|b| std::cmp::Reverse(b.saved_at));
    tracks
}

/// Download `track` from the signed-in Subsonic server and keep it for offline playback.
pub async fn save(track: &Track) -> Result<CachedTrack> {
    let id = track.id.as_deref().context("the track has no id")?;
    clear_cancelled(id);
    let Some(client) = client()? else {
        bail!("sign in to a Subsonic server to save tracks offline");
    };
    let server = server_key().context("sign in to a Subsonic server to save tracks offline")?;

    let dir = root_for(&server);
    fs::create_dir_all(&dir).context("cannot create the offline cache folder")?;
    let dest = file_for_in(&dir, id);
    let tmp = dest.with_extension("part");
    if is_cancelled(id) {
        bail!("download cancelled");
    }

    let response = client.open_download(id).await?;
    if is_cancelled(id) {
        bail!("download cancelled");
    }
    let mut response = response;
    let mut written: u64 = 0;
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        while let Some(chunk) = response.chunk().await.context("download stream broke")? {
            if is_cancelled(id) {
                drop(file);
                let _ = fs::remove_file(&tmp);
                bail!("download cancelled");
            }
            if chunk.is_empty() {
                continue;
            }
            file.write_all(&chunk)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            written += chunk.len() as u64;
        }
        file.sync_all()?;
    }
    if written == 0 {
        let _ = fs::remove_file(&tmp);
        bail!("the server returned an empty file for {id}");
    }
    // Re-check after the body is on disk: remove may have raced the write.
    if is_cancelled(id) {
        let _ = fs::remove_file(&tmp);
        bail!("download cancelled");
    }
    fs::rename(&tmp, &dest).with_context(|| format!("cannot finalize {}", dest.display()))?;
    if is_cancelled(id) {
        let _ = fs::remove_file(&dest);
        bail!("download cancelled");
    }

    let entry = CachedTrack {
        id: id.to_owned(),
        name: track.name.clone(),
        artists: track.artists.clone(),
        album: track.album.clone(),
        duration_ms: u64::try_from(track.duration.as_millis()).unwrap_or(0),
        bytes: written,
        saved_at: now(),
        file: dest
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("track.audio")
            .to_owned(),
    };

    let mut guard = held().lock().expect("offline cache lock");
    if guard.key != server {
        guard.manifest = load_manifest_for(&server);
        guard.key = server.clone();
    }
    guard.manifest.version = 1;
    guard.manifest.server = server.clone();
    guard.manifest.tracks.insert(id.to_owned(), entry.clone());
    persist(&server, &guard.manifest)?;
    Ok(entry)
}

pub fn remove(track_id: &str) -> Result<()> {
    cancel(track_id);
    remove_file_only(track_id)?;
    let mut guard = held().lock().expect("offline cache lock");
    let Some(key) = ensure(&mut guard) else {
        return Ok(());
    };
    guard.manifest.tracks.remove(track_id);
    persist(&key, &guard.manifest)?;
    Ok(())
}

fn remove_file_only(track_id: &str) -> Result<()> {
    let Some(path) = file_for(track_id) else {
        return Ok(());
    };
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
    }
    if let Ok(mut guard) = held().lock()
        && let Some(key) = ensure(&mut guard)
        && let Some(entry) = guard.manifest.tracks.get(track_id)
    {
        let named = root_for(&key).join(&entry.file);
        if named.exists() && named != path {
            let _ = fs::remove_file(named);
        }
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

/// Path of a ready offline file for playback without loading it into RAM.
pub fn cached_file(track_id: &str) -> Option<PathBuf> {
    let path = path(track_id)?;
    let meta = fs::metadata(&path).ok()?;
    (meta.is_file() && meta.len() > 0).then_some(path)
}

/// Length recorded when the track was saved, so playback does not need the network.
pub fn cached_duration(track_id: &str) -> Option<Duration> {
    let Ok(mut guard) = held().lock() else {
        return None;
    };
    ensure(&mut guard)?;
    guard
        .manifest
        .tracks
        .get(track_id)
        .map(|entry| Duration::from_millis(entry.duration_ms))
}

/// Whether `path` is inside the offline cache tree (used by diagnostics).
pub fn contains(path: &Path) -> bool {
    path.starts_with(base())
}
