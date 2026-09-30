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
    /// Server the cache belongs to. A different server clears the index on next save.
    server: String,
    tracks: HashMap<String, CachedTrack>,
}

fn root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join("offline")
        .join("subsonic")
}

fn manifest_path() -> PathBuf {
    root().join(MANIFEST)
}

fn file_for(id: &str) -> PathBuf {
    // Hash ids that contain path separators so a server id cannot escape the cache dir.
    let safe = match id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        true => id.to_owned(),
        false => format!("{:x}", Sha256::digest(id.as_bytes())),
    };
    root().join(format!("{safe}.{AUDIO_EXT}"))
}

fn held() -> &'static Mutex<Manifest> {
    static HELD: OnceLock<Mutex<Manifest>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(load_manifest()))
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
    let part = file_for(track_id).with_extension("part");
    if part.exists() {
        let _ = fs::remove_file(&part);
    }
}

fn load_manifest() -> Manifest {
    let path = manifest_path();
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Manifest {
            version: 1,
            ..Manifest::default()
        },
    }
}

fn persist(manifest: &Manifest) -> Result<()> {
    let dir = root();
    fs::create_dir_all(&dir).context("cannot create the offline cache folder")?;
    let path = manifest_path();
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
    let server = server_key()?;
    let Ok(guard) = held().lock() else {
        return None;
    };
    if guard.server != server {
        return None;
    }
    let entry = guard.tracks.get(track_id)?;
    let path = root().join(&entry.file);
    path.is_file().then_some(path)
}

pub fn list() -> Vec<CachedTrack> {
    let server = match server_key() {
        Some(server) => server,
        None => return Vec::new(),
    };
    let Ok(guard) = held().lock() else {
        return Vec::new();
    };
    if guard.server != server {
        return Vec::new();
    }
    let mut tracks: Vec<_> = guard.tracks.values().cloned().collect();
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

    let bytes = client.download_track(id).await?;
    if is_cancelled(id) {
        bail!("download cancelled");
    }
    if bytes.is_empty() {
        bail!("the server returned an empty file for {id}");
    }

    let dir = root();
    fs::create_dir_all(&dir).context("cannot create the offline cache folder")?;
    let dest = file_for(id);
    let tmp = dest.with_extension("part");
    if is_cancelled(id) {
        bail!("download cancelled");
    }
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
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
        bytes: bytes.len() as u64,
        saved_at: now(),
        file: dest
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("track.audio")
            .to_owned(),
    };

    // Switch libraries: drop every old file before rewriting the index.
    let stale = {
        let guard = held().lock().expect("offline cache lock");
        if guard.server != server {
            guard.tracks.keys().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };
    for old in &stale {
        let _ = remove_file_only(old);
    }

    let mut guard = held().lock().expect("offline cache lock");
    if !stale.is_empty() {
        guard.tracks.clear();
    }
    guard.version = 1;
    guard.server = server;
    guard.tracks.insert(id.to_owned(), entry.clone());
    persist(&guard)?;
    Ok(entry)
}

pub fn remove(track_id: &str) -> Result<()> {
    cancel(track_id);
    remove_file_only(track_id)?;
    let mut guard = held().lock().expect("offline cache lock");
    guard.tracks.remove(track_id);
    persist(&guard)?;
    Ok(())
}

fn remove_file_only(track_id: &str) -> Result<()> {
    let path = file_for(track_id);
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
    }
    if let Ok(guard) = held().lock()
        && let Some(entry) = guard.tracks.get(track_id)
    {
        let named = root().join(&entry.file);
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

/// Read a cached file's bytes for playback. Returns `None` when the track is not saved.
pub fn read_cached(track_id: &str) -> Option<(PathBuf, Vec<u8>)> {
    let path = path(track_id)?;
    let bytes = fs::read(&path).ok()?;
    (!bytes.is_empty()).then_some((path, bytes))
}

/// Length recorded when the track was saved, so playback does not need the network.
pub fn cached_duration(track_id: &str) -> Option<Duration> {
    let server = server_key()?;
    let Ok(guard) = held().lock() else {
        return None;
    };
    if guard.server != server {
        return None;
    }
    guard
        .tracks
        .get(track_id)
        .map(|entry| Duration::from_millis(entry.duration_ms))
}

/// Whether `path` is inside the offline cache directory (used by diagnostics).
pub fn contains(path: &Path) -> bool {
    path.starts_with(root())
}
