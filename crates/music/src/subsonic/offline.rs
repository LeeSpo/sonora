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

use crate::subsonic::auth;
use crate::subsonic::client::SubsonicClient;
use crate::{ArtistRef, MusicApi as _, Track};

const MANIFEST: &str = "manifest.json";
const AUDIO_EXT: &str = "audio";
/// Sidecar with the lyrics kept for a saved track (`{id}.lyrics.json`).
const LYRICS_EXT: &str = "lyrics.json";
/// Legacy per-track cover sidecar (`{id}.cover`). Migrated into `covers/` on load.
const COVER_EXT: &str = "cover";
/// Directory under the account root that holds one cover per album (or per track with no album).
const COVER_DIR: &str = "covers";
/// Extension of the kept cover file after downscale (always JPEG).
const COVER_FILE_EXT: &str = "jpg";
/// Longest edge kept for an offline cover, in pixels.
const COVER_MAX: u32 = 600;
/// JPEG quality for a kept offline cover.
const COVER_QUALITY: u8 = 85;
/// Prefix of the stand-in artist id a saved track carries when its row predates artist ids.
/// The rest of the id is the artist's name, so the Downloaded screen can still group by it.
pub const NAME_ARTIST_PREFIX: &str = "offline-artist:";
/// How one display line of artists separates the names in it: our own join in `wire`, the
/// bullet Navidrome uses, and the two other separators tag editors commonly write.
const ARTIST_SEPARATORS: [&str; 4] = [", ", " \u{2022} ", "; ", " / "];

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
    /// Album id, so the Downloaded list can link to the album.
    #[serde(default)]
    pub album_id: Option<String>,
    /// Remote cover url remembered at save time; used when the sidecar image is missing.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// The credited artists with their server ids, so the artist line links offline too. Rows
    /// saved before ids were kept have none until `backfill` or a play fills them in.
    #[serde(default)]
    pub artist_refs: Vec<ArtistRef>,
    /// Place on the album, so a saved album plays in order. Zero when unknown.
    #[serde(default)]
    pub track_number: u32,
    #[serde(default)]
    pub disc_number: u32,
}

/// What `backfill` learned from one batch: the rows that changed, and every id the server
/// answered for, changed or not, so a caller does not ask about it again.
#[derive(Clone, Debug, Default)]
pub struct Backfill {
    pub changed: Vec<CachedTrack>,
    pub answered: Vec<String>,
}

/// Lyrics kept beside a saved track so they show without the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedLyrics {
    /// Provider name the sheet came from (e.g. `Subsonic`, `LRCLIB`).
    pub source: String,
    pub lyrics: crate::Lyrics,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
}

/// How much space the signed-in account's offline saves take on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub tracks: usize,
    /// Audio + covers + lyrics sidecars.
    pub bytes: u64,
    pub audio_bytes: u64,
    pub cover_bytes: u64,
    pub lyrics_bytes: u64,
}

/// One album among the saved tracks, with how much space its copies take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumGroup {
    pub album_id: Option<String>,
    pub name: String,
    pub artists: String,
    pub cover: Option<String>,
    pub track_count: usize,
    pub bytes: u64,
    pub track_ids: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Account key (`Credentials::account_key`) this directory belongs to.
    server: String,
    tracks: HashMap<String, CachedTrack>,
}

fn base() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = test_root() {
        return root;
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join("offline")
        .join("subsonic")
}

#[cfg(test)]
fn test_root() -> Option<PathBuf> {
    TEST_ROOT.lock().ok().and_then(|guard| guard.clone())
}

#[cfg(test)]
fn test_key() -> Option<String> {
    TEST_KEY.lock().ok().and_then(|guard| guard.clone())
}

#[cfg(test)]
static TEST_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
static TEST_KEY: Mutex<Option<String>> = Mutex::new(None);

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

fn safe_name(id: &str) -> String {
    // Hash ids that contain path separators so a server id cannot escape the cache dir.
    match id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        true => id.to_owned(),
        false => format!("{:x}", Sha256::digest(id.as_bytes())),
    }
}

fn file_for_in(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.{AUDIO_EXT}", safe_name(id)))
}

fn sidecar_in(dir: &Path, id: &str, ext: &str) -> PathBuf {
    dir.join(format!("{}.{ext}", safe_name(id)))
}

fn covers_dir(dir: &Path) -> PathBuf {
    dir.join(COVER_DIR)
}

/// Stable cover file key: album id when present, otherwise the track id.
fn cover_key(album_id: Option<&str>, track_id: &str) -> String {
    match album_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(album) => safe_name(album),
        None => safe_name(track_id),
    }
}

fn cover_path_in(dir: &Path, key: &str) -> PathBuf {
    covers_dir(dir).join(format!("{key}.{COVER_FILE_EXT}"))
}

fn legacy_cover_in(dir: &Path, track_id: &str) -> PathBuf {
    sidecar_in(dir, track_id, COVER_EXT)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Downscale `bytes` to at most [`COVER_MAX`] on the long edge and write a JPEG to `dest`.
fn write_cover_bytes(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).context("cannot create the offline covers folder")?;
    }
    let image = image::load_from_memory(bytes).context("cannot decode the cover image")?;
    let image = if image.width() > COVER_MAX || image.height() > COVER_MAX {
        image.thumbnail(COVER_MAX, COVER_MAX)
    } else {
        image
    };
    let rgb = image.to_rgb8();
    let mut encoded = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, COVER_QUALITY);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .context("cannot encode the cover as JPEG")?;
    let tmp = dest.with_extension("jpg.tmp");
    fs::write(&tmp, &encoded).context("cannot write the cover")?;
    fs::rename(&tmp, dest).context("cannot replace the cover")?;
    Ok(())
}

/// Keep a cover for `track` under the album (or track) key. Best effort.
fn keep_cover(dir: &Path, track_id: &str, album_id: Option<&str>, bytes: &[u8]) {
    let key = cover_key(album_id, track_id);
    let dest = cover_path_in(dir, &key);
    if dest.is_file() && file_size(&dest) > 0 {
        return;
    }
    if let Err(error) = write_cover_bytes(&dest, bytes) {
        log::debug!("offline: cannot keep the cover for {track_id}: {error:#}");
    }
}

/// How many saved tracks still share the cover keyed by `album_id` / `track_id`.
fn cover_holders(manifest: &Manifest, album_id: Option<&str>, track_id: &str) -> usize {
    let key = cover_key(album_id, track_id);
    manifest
        .tracks
        .values()
        .filter(|entry| cover_key(entry.album_id.as_deref(), &entry.id) == key)
        .count()
}

/// Drop the cover file when nothing left references it. Also clears a legacy per-track sidecar.
fn drop_cover_for(dir: &Path, manifest: &Manifest, album_id: Option<&str>, track_id: &str) {
    let legacy = legacy_cover_in(dir, track_id);
    if legacy.exists() {
        let _ = fs::remove_file(&legacy);
    }
    if cover_holders(manifest, album_id, track_id) > 0 {
        return;
    }
    let path = cover_path_in(dir, &cover_key(album_id, track_id));
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
}

/// Move legacy `{id}.cover` sidecars into `covers/{key}.jpg`, deleting the duplicates.
fn migrate_covers(dir: &Path, manifest: &Manifest) {
    for entry in manifest.tracks.values() {
        let legacy = legacy_cover_in(dir, &entry.id);
        if !legacy.is_file() {
            continue;
        }
        let dest = cover_path_in(dir, &cover_key(entry.album_id.as_deref(), &entry.id));
        if !dest.is_file() || file_size(&dest) == 0 {
            match fs::read(&legacy) {
                Ok(bytes) if !bytes.is_empty() => {
                    if let Err(error) = write_cover_bytes(&dest, &bytes) {
                        log::debug!(
                            "offline: cannot migrate cover for {}: {error:#}",
                            entry.id
                        );
                        continue;
                    }
                }
                _ => continue,
            }
        }
        let _ = fs::remove_file(&legacy);
    }
}

/// Cover image for a saved track as a `file://` url: per-album file first, then legacy sidecar.
fn cover_url_in(dir: &Path, track_id: &str, album_id: Option<&str>) -> Option<String> {
    let album = cover_path_in(dir, &cover_key(album_id, track_id));
    if let Ok(meta) = fs::metadata(&album)
        && meta.is_file()
        && meta.len() > 0
    {
        return Some(format!("file://{}", album.display()));
    }
    let legacy = legacy_cover_in(dir, track_id);
    let meta = fs::metadata(&legacy).ok()?;
    (meta.is_file() && meta.len() > 0).then(|| format!("file://{}", legacy.display()))
}

fn bytes_under(dir: &Path, relative: &str) -> u64 {
    file_size(&dir.join(relative))
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
    let mut manifest = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| Manifest {
            version: 1,
            server: account_key.to_owned(),
            ..Manifest::default()
        }),
        Err(_) => Manifest {
            version: 1,
            server: account_key.to_owned(),
            ..Manifest::default()
        },
    };
    if manifest.server.is_empty() {
        manifest.server = account_key.to_owned();
    }
    if validate(account_key, &mut manifest) {
        if let Err(error) = persist(account_key, &manifest) {
            log::warn!("offline: cannot rewrite the validated index: {error:#}");
        }
    }
    manifest
}

/// Drop manifest rows whose files are gone, and delete orphan `.audio` / `.part` files that
/// are not indexed. Returns whether the manifest changed.
fn validate(account_key: &str, manifest: &mut Manifest) -> bool {
    let dir = root_for(account_key);
    let mut dirty = false;
    let before = manifest.tracks.len();
    manifest.tracks.retain(|_id, entry| {
        let path = dir.join(&entry.file);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => true,
            _ => {
                dirty = true;
                false
            }
        }
    });
    if manifest.tracks.len() != before {
        dirty = true;
    }

    // Fold old per-track covers into one file per album before sweeping orphans.
    migrate_covers(&dir, manifest);

    let known: HashSet<String> = manifest
        .tracks
        .values()
        .map(|entry| entry.file.clone())
        .collect();
    let cover_keys: HashSet<String> = manifest
        .tracks
        .values()
        .map(|entry| cover_key(entry.album_id.as_deref(), &entry.id))
        .collect();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == MANIFEST || name == COVER_DIR || name.ends_with(".json.tmp") {
                continue;
            }
            let path = entry.path();
            if name.ends_with(".part") {
                let _ = fs::remove_file(&path);
                continue;
            }
            if name.ends_with(&format!(".{AUDIO_EXT}")) && !known.contains(name) {
                log::info!("offline: removing orphan cache file {name}");
                let _ = fs::remove_file(&path);
                continue;
            }
            // Sidecars live and die with their audio file.
            for ext in [LYRICS_EXT, COVER_EXT] {
                if let Some(stem) = name.strip_suffix(&format!(".{ext}"))
                    && !known.contains(&format!("{stem}.{AUDIO_EXT}"))
                {
                    log::info!("offline: removing orphan sidecar {name}");
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
    // Drop cover files no album still references.
    let covers = covers_dir(&dir);
    if let Ok(entries) = fs::read_dir(&covers) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(&format!(".{COVER_FILE_EXT}")) else {
                continue;
            };
            if !cover_keys.contains(stem) {
                log::info!("offline: removing orphan cover {name}");
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    dirty
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
    #[cfg(test)]
    if let Some(key) = test_key() {
        return Some(key);
    }
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
    let Ok(mut guard) = held().lock() else {
        return false;
    };
    ensure(&mut guard).is_some() && !guard.manifest.tracks.is_empty()
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

    // Best effort: the audio is what matters, so a missing cover or lyric never fails the save.
    // One cover per album (keyed by album id, or the track id when there is none).
    if let Some(url) = track.cover.as_deref().filter(|url| url.starts_with("http")) {
        match client.fetch_bytes(url).await {
            Ok(bytes) if !bytes.is_empty() => {
                keep_cover(&dir, id, track.album_id.as_deref(), &bytes);
            }
            Ok(_) => {}
            Err(error) => log::debug!("offline: cannot fetch the cover for {id}: {error:#}"),
        }
    }
    if !sidecar_in(&dir, id, LYRICS_EXT).is_file() {
        match server_lyrics(track).await {
            Ok(Some(found)) => {
                if let Err(error) = write_lyrics_in(&dir, id, &found) {
                    log::debug!("offline: cannot keep lyrics for {id}: {error:#}");
                }
            }
            Ok(None) => {}
            Err(error) => log::debug!("offline: cannot fetch lyrics for {id}: {error:#}"),
        }
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
        album_id: track.album_id.clone(),
        cover_url: track.cover.clone().filter(|url| url.starts_with("http")),
        artist_refs: server_refs(&track.artist_refs),
        track_number: track.track_number,
        disc_number: track.disc_number,
    };

    let mut guard = held().lock().expect("offline cache lock");
    if guard.key != server {
        guard.manifest = load_manifest_for(&server);
        guard.key = server.clone();
    }
    let previous = guard.manifest.tracks.insert(id.to_owned(), entry.clone());
    guard.manifest.version = 1;
    guard.manifest.server = server.clone();
    if let Err(error) = persist(&server, &guard.manifest) {
        // Roll back the index and the new file so a failed save leaves nothing half-written.
        match previous {
            Some(previous) => {
                guard.manifest.tracks.insert(id.to_owned(), previous);
            }
            None => {
                guard.manifest.tracks.remove(id);
            }
        }
        let _ = fs::remove_file(&dest);
        return Err(error).context("cannot update the offline index");
    }
    Ok(entry)
}

pub fn remove(track_id: &str) -> Result<()> {
    cancel(track_id);
    let mut guard = held().lock().expect("offline cache lock");
    let Some(key) = ensure(&mut guard) else {
        // Nothing signed in: still try to drop a leftover file for this id.
        drop(guard);
        remove_files_for(track_id, None);
        return Ok(());
    };
    let Some(entry) = guard.manifest.tracks.remove(track_id) else {
        // Not indexed: clear any stray file named for this id.
        drop(guard);
        remove_files_for(track_id, Some(&key));
        return Ok(());
    };
    if let Err(error) = persist(&key, &guard.manifest) {
        // Roll the row back so the index still matches the file on disk.
        guard.manifest.tracks.insert(track_id.to_owned(), entry);
        return Err(error).context("cannot update the offline index");
    }
    let named = root_for(&key).join(&entry.file);
    let album_id = entry.album_id.clone();
    let dir = root_for(&key);
    // Cover may still be shared by other tracks; decide after the row is gone.
    drop_cover_for(&dir, &guard.manifest, album_id.as_deref(), track_id);
    drop(guard);
    // Index already committed: a file delete failure leaves an orphan that validate sweeps.
    if named.exists()
        && let Err(error) = fs::remove_file(&named)
    {
        log::warn!("offline: cannot delete {}: {error}", named.display());
    }
    remove_files_for(track_id, Some(&key));
    Ok(())
}

fn remove_files_for(track_id: &str, account_key: Option<&str>) {
    let mut dirs = Vec::new();
    if let Some(dir) = root() {
        dirs.push(dir);
    }
    if let Some(key) = account_key {
        dirs.push(root_for(key));
    }
    for dir in dirs {
        // Album covers are shared; `drop_cover_for` removes them when the last track goes.
        // Only the legacy per-track sidecar is cleared here.
        for path in [
            file_for_in(&dir, track_id),
            sidecar_in(&dir, track_id, LYRICS_EXT),
            sidecar_in(&dir, track_id, COVER_EXT),
        ] {
            if path.exists() {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Cover image kept for a saved track, as a `file://` url the artwork loader reads.
/// Prefers the per-album file under `covers/`, then a legacy per-track sidecar.
pub fn cover(track_id: &str) -> Option<String> {
    let Ok(mut guard) = held().lock() else {
        return None;
    };
    let key = ensure(&mut guard)?;
    let dir = root_for(&key);
    let album_id = guard
        .manifest
        .tracks
        .get(track_id)
        .and_then(|entry| entry.album_id.clone());
    cover_url_in(&dir, track_id, album_id.as_deref())
}

/// Lyrics kept beside a saved track, if any were found when it was saved or played.
pub fn lyrics(track_id: &str) -> Option<CachedLyrics> {
    let path = sidecar_in(&root()?, track_id, LYRICS_EXT);
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Replace the lyrics kept for a saved track (e.g. with the sheet that won online). Does
/// nothing for a track that is not saved, so the cache never grows lyric-only rows.
pub fn store_lyrics(track_id: &str, found: &CachedLyrics) -> Result<()> {
    if !is_cached(track_id) {
        return Ok(());
    }
    let dir = root().context("sign in to a Subsonic server to save lyrics offline")?;
    write_lyrics_in(&dir, track_id, found)
}

fn write_lyrics_in(dir: &Path, track_id: &str, found: &CachedLyrics) -> Result<()> {
    let path = sidecar_in(dir, track_id, LYRICS_EXT);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(found).context("cannot serialize the lyrics")?;
    fs::write(&tmp, bytes).context("cannot write the lyrics")?;
    fs::rename(&tmp, &path).context("cannot replace the lyrics")?;
    Ok(())
}

/// The server's own best sheet for `track` (`getLyricsBySongId`), ranked like playback does.
async fn server_lyrics(track: &Track) -> Result<Option<CachedLyrics>> {
    use crate::LyricsProvider as _;
    let Some(id) = track.id.clone() else {
        return Ok(None);
    };
    let query = crate::LyricsQuery {
        title: track.name.clone(),
        artist: track.artists.clone(),
        album: (!track.album.is_empty()).then(|| track.album.clone()),
        duration: track.duration,
        track: Some(crate::TrackKey {
            provider: "subsonic",
            id,
        }),
    };
    let hits = crate::subsonic::SubsonicLyrics::new()
        .search(&query)
        .await?;
    let ranked = crate::lyrics::rank(&query, hits);
    Ok(ranked.into_iter().next().map(|hit| CachedLyrics {
        source: hit.source.to_owned(),
        lyrics: hit.lyrics,
        instrumental: hit.instrumental,
        title: hit.title,
        artist: hit.artist,
    }))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

/// Ids of saved rows that carry no artist ids yet, so `backfill` knows what to ask about.
pub fn missing_artists() -> Vec<String> {
    let Ok(mut guard) = held().lock() else {
        return Vec::new();
    };
    if ensure(&mut guard).is_none() {
        return Vec::new();
    }
    let mut missing: Vec<(i64, String)> = guard
        .manifest
        .tracks
        .values()
        .filter(|entry| entry.artist_refs.is_empty())
        .map(|entry| (entry.saved_at, entry.id.clone()))
        .collect();
    missing.sort_by_key(|(saved_at, _)| std::cmp::Reverse(*saved_at));
    missing.into_iter().map(|(_, id)| id).collect()
}

/// Copy what `tracks` say about their artists and album position onto their saved rows, and
/// write the index once. Only a track with at least one server artist id counts; the rows
/// that changed come back so the caller can refresh what it shows.
pub fn remember(tracks: &[Track]) -> Vec<CachedTrack> {
    let Ok(mut guard) = held().lock() else {
        return Vec::new();
    };
    let Some(key) = ensure(&mut guard) else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    let mut previous = Vec::new();
    for track in tracks {
        let Some(id) = track.id.as_deref() else {
            continue;
        };
        let refs = server_refs(&track.artist_refs);
        let Some(entry) = guard.manifest.tracks.get_mut(id) else {
            continue;
        };
        if refs.is_empty() || entry.artist_refs == refs {
            continue;
        }
        previous.push(entry.clone());
        entry.artist_refs = refs;
        if track.track_number > 0 {
            entry.track_number = track.track_number;
        }
        if track.disc_number > 0 {
            entry.disc_number = track.disc_number;
        }
        changed.push(entry.clone());
    }
    if changed.is_empty() {
        return changed;
    }
    if let Err(error) = persist(&key, &guard.manifest) {
        log::warn!("offline: cannot record artist ids: {error:#}");
        for entry in previous {
            guard.manifest.tracks.insert(entry.id.clone(), entry);
        }
        return Vec::new();
    }
    changed
}

/// Ask the server about saved tracks whose rows predate artist ids, a few at a time, and
/// record the ids it hands back. Fails only when no track at all could be asked about, which
/// is what a lost connection looks like.
pub async fn backfill(ids: Vec<String>) -> Result<Backfill> {
    let Some(client) = client()? else {
        bail!("sign in to a Subsonic server to look up saved tracks");
    };
    let asked = futures::future::join_all(ids.iter().map(|id| client.track(id))).await;
    let mut found = Vec::new();
    let mut answered = Vec::new();
    let mut first_error = None;
    for (id, result) in ids.iter().zip(asked) {
        match result {
            Ok(track) => {
                answered.push(id.clone());
                found.push(track);
            }
            Err(error) => {
                log::debug!("offline: cannot look up {id}: {error:#}");
                first_error.get_or_insert(error);
            }
        }
    }
    if answered.is_empty()
        && let Some(error) = first_error
    {
        return Err(error.context("cannot look up saved tracks"));
    }
    Ok(Backfill {
        changed: remember(&found),
        answered,
    })
}

/// A server id of a saved track's artist, among the ids of the artists it is credited to,
/// whose name matches `name`. Lets a stand-in name link become the real artist page online.
pub fn artist_id_for_name(name: &str) -> Option<String> {
    let Ok(mut guard) = held().lock() else {
        return None;
    };
    ensure(&mut guard)?;
    guard
        .manifest
        .tracks
        .values()
        .flat_map(|entry| entry.artist_refs.iter())
        .filter(|artist| same_name(&artist.name, name))
        .find_map(|artist| artist.id.clone().filter(|id| !is_name_artist_id(id)))
}

/// The artists a saved row credits: its stored ids, or, for a row saved before those were
/// kept, the names in its display line, each under a stand-in id from `name_artist_id`.
pub fn credited(entry: &CachedTrack) -> Vec<ArtistRef> {
    if !entry.artist_refs.is_empty() {
        return entry.artist_refs.clone();
    }
    split_artists(&entry.artists)
        .into_iter()
        .map(|name| ArtistRef {
            id: Some(name_artist_id(&name)),
            name,
        })
        .collect()
}

/// The names in a display line of artists, split on the separators players write between
/// them. A line without one is a single name.
pub fn split_artists(display: &str) -> Vec<String> {
    let mut names = vec![display.to_owned()];
    for separator in ARTIST_SEPARATORS {
        names = names
            .iter()
            .flat_map(|name| name.split(separator))
            .map(str::to_owned)
            .collect();
    }
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether two artist names are the same artist, ignoring case.
pub fn same_name(left: &str, right: &str) -> bool {
    left == right || left.to_lowercase() == right.to_lowercase()
}

/// The stand-in artist id for `name` on a row that has no server ids.
pub fn name_artist_id(name: &str) -> String {
    format!("{NAME_ARTIST_PREFIX}{name}")
}

/// Whether `id` is a stand-in from `name_artist_id` rather than a server id.
pub fn is_name_artist_id(id: &str) -> bool {
    id.starts_with(NAME_ARTIST_PREFIX)
}

/// The artist name inside a stand-in id, or `None` for a server id.
pub fn name_of_artist_id(id: &str) -> Option<&str> {
    id.strip_prefix(NAME_ARTIST_PREFIX)
}

/// The refs worth keeping in the index: only server ids count, so a track that carries
/// stand-ins or no ids at all is stored with none and `backfill` picks it up later.
fn server_refs(refs: &[ArtistRef]) -> Vec<ArtistRef> {
    let known = refs.iter().any(|artist| {
        artist
            .id
            .as_deref()
            .is_some_and(|id| !is_name_artist_id(id))
    });
    match known {
        true => refs
            .iter()
            .map(|artist| ArtistRef {
                name: artist.name.clone(),
                id: artist.id.clone().filter(|id| !is_name_artist_id(id)),
            })
            .collect(),
        false => Vec::new(),
    }
}

/// How much space the signed-in account's offline saves take (audio + covers + lyrics).
pub fn usage() -> Usage {
    let Ok(mut guard) = held().lock() else {
        return Usage::default();
    };
    let Some(key) = ensure(&mut guard) else {
        return Usage::default();
    };
    usage_of(&root_for(&key), &guard.manifest)
}

fn usage_of(dir: &Path, manifest: &Manifest) -> Usage {
    let mut audio_bytes = 0u64;
    let mut lyrics_bytes = 0u64;
    for entry in manifest.tracks.values() {
        audio_bytes += bytes_under(dir, &entry.file);
        lyrics_bytes += file_size(&sidecar_in(dir, &entry.id, LYRICS_EXT));
    }
    let mut cover_bytes = 0u64;
    let mut seen = HashSet::new();
    for entry in manifest.tracks.values() {
        let key = cover_key(entry.album_id.as_deref(), &entry.id);
        if !seen.insert(key.clone()) {
            continue;
        }
        cover_bytes += file_size(&cover_path_in(dir, &key));
        // A still-unmigrated legacy sidecar counts until migrate_covers runs.
        cover_bytes += file_size(&legacy_cover_in(dir, &entry.id));
    }
    Usage {
        tracks: manifest.tracks.len(),
        bytes: audio_bytes + cover_bytes + lyrics_bytes,
        audio_bytes,
        cover_bytes,
        lyrics_bytes,
    }
}

/// Bytes on disk for the given track ids (audio + their lyrics; covers counted once per album).
pub fn size_of(track_ids: &[String]) -> u64 {
    let Ok(mut guard) = held().lock() else {
        return 0;
    };
    let Some(key) = ensure(&mut guard) else {
        return 0;
    };
    let dir = root_for(&key);
    let mut total = 0u64;
    let mut covers = HashSet::new();
    for id in track_ids {
        let Some(entry) = guard.manifest.tracks.get(id) else {
            continue;
        };
        total += bytes_under(&dir, &entry.file);
        total += file_size(&sidecar_in(&dir, &entry.id, LYRICS_EXT));
        let cover = cover_key(entry.album_id.as_deref(), &entry.id);
        if covers.insert(cover.clone()) {
            total += file_size(&cover_path_in(&dir, &cover));
            total += file_size(&legacy_cover_in(&dir, &entry.id));
        }
    }
    total
}

/// Saved albums with song count and bytes, newest album first (by latest saved_at in it).
pub fn albums() -> Vec<AlbumGroup> {
    let Ok(mut guard) = held().lock() else {
        return Vec::new();
    };
    let Some(key) = ensure(&mut guard) else {
        return Vec::new();
    };
    albums_of(&root_for(&key), &guard.manifest)
}

fn albums_of(dir: &Path, manifest: &Manifest) -> Vec<AlbumGroup> {
    let mut groups: HashMap<String, AlbumGroup> = HashMap::new();
    let mut latest: HashMap<String, i64> = HashMap::new();
    for entry in manifest.tracks.values() {
        if entry.album.trim().is_empty() && entry.album_id.is_none() {
            continue;
        }
        let group_key = entry
            .album_id
            .clone()
            .unwrap_or_else(|| format!("name:{}", entry.album.to_lowercase()));
        let audio = bytes_under(dir, &entry.file);
        let lyrics = file_size(&sidecar_in(dir, &entry.id, LYRICS_EXT));
        let cover_key_now = cover_key(entry.album_id.as_deref(), &entry.id);
        let cover_bytes = match groups.contains_key(&group_key) {
            true => 0,
            false => {
                file_size(&cover_path_in(dir, &cover_key_now))
                    + file_size(&legacy_cover_in(dir, &entry.id))
            }
        };
        let slot = groups.entry(group_key.clone()).or_insert_with(|| AlbumGroup {
            album_id: entry.album_id.clone(),
            name: entry.album.clone(),
            artists: entry.artists.clone(),
            cover: cover_url_in(dir, &entry.id, entry.album_id.as_deref()),
            track_count: 0,
            bytes: 0,
            track_ids: Vec::new(),
        });
        if slot.cover.is_none() {
            slot.cover = cover_url_in(dir, &entry.id, entry.album_id.as_deref());
        }
        if slot.artists.is_empty() {
            slot.artists = entry.artists.clone();
        }
        slot.track_count += 1;
        slot.bytes += audio + lyrics + cover_bytes;
        slot.track_ids.push(entry.id.clone());
        let seen = latest.entry(group_key).or_insert(entry.saved_at);
        *seen = (*seen).max(entry.saved_at);
    }
    let mut albums: Vec<_> = groups.into_iter().map(|(key, group)| (latest[&key], group)).collect();
    albums.sort_by_key(|(saved, group)| (std::cmp::Reverse(*saved), group.name.to_lowercase()));
    albums.into_iter().map(|(_, group)| group).collect()
}

/// Track ids saved under `album_id`, or under the album name when `album_id` is `None`.
pub fn tracks_for_album(album_id: Option<&str>, album_name: &str) -> Vec<String> {
    let Ok(mut guard) = held().lock() else {
        return Vec::new();
    };
    if ensure(&mut guard).is_none() {
        return Vec::new();
    }
    guard
        .manifest
        .tracks
        .values()
        .filter(|entry| match album_id {
            Some(want) => entry.album_id.as_deref() == Some(want),
            None => same_name(&entry.album, album_name),
        })
        .map(|entry| entry.id.clone())
        .collect()
}

/// Human-readable size for the UI (e.g. `3.2 GB`, `450 MB`, `12 KB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_written_before_artist_ids_still_loads() {
        let json = r#"{
            "version": 1,
            "server": "account",
            "tracks": {
                "t1": {
                    "id": "t1",
                    "name": "Song",
                    "artists": "Alpha, Beta",
                    "album": "Record",
                    "duration_ms": 1000,
                    "bytes": 10,
                    "saved_at": 5,
                    "file": "t1.audio",
                    "album_id": "al1"
                }
            }
        }"#;
        let manifest: Manifest = serde_json::from_str(json).expect("old index parses");
        let entry = &manifest.tracks["t1"];

        assert!(entry.artist_refs.is_empty());
        assert_eq!(entry.track_number, 0);
        assert_eq!(entry.cover_url, None);
        let names: Vec<_> = credited(entry).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Alpha", "Beta"]);
    }

    #[test]
    fn stored_ids_round_trip_and_win_over_the_display_line() {
        let entry = CachedTrack {
            id: "t1".into(),
            name: "Song".into(),
            artists: "Alpha feat. Beta".into(),
            album: "Record".into(),
            duration_ms: 1,
            bytes: 1,
            saved_at: 1,
            file: "t1.audio".into(),
            album_id: None,
            cover_url: None,
            artist_refs: vec![ArtistRef {
                name: "Alpha".into(),
                id: Some("ar1".into()),
            }],
            track_number: 3,
            disc_number: 1,
        };
        let json = serde_json::to_string(&entry).expect("serializes");
        let back: CachedTrack = serde_json::from_str(&json).expect("parses");

        assert_eq!(back.artist_refs, entry.artist_refs);
        assert_eq!(back.track_number, 3);
        assert_eq!(credited(&back), entry.artist_refs);
    }

    #[test]
    fn a_display_line_splits_into_stand_in_artists() {
        assert_eq!(
            split_artists("Alpha, Beta \u{2022} Gamma; Delta / Epsilon"),
            ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"]
        );
        assert_eq!(split_artists("Simon & Garfunkel"), ["Simon & Garfunkel"]);
        assert!(split_artists("  ").is_empty());

        let id = name_artist_id("Alpha");
        assert!(is_name_artist_id(&id));
        assert_eq!(name_of_artist_id(&id), Some("Alpha"));
        assert_eq!(name_of_artist_id("ar1"), None);
    }

    #[test]
    fn only_server_ids_are_kept_in_the_index() {
        let stand_in = ArtistRef {
            name: "Alpha".into(),
            id: Some(name_artist_id("Alpha")),
        };
        assert!(server_refs(std::slice::from_ref(&stand_in)).is_empty());
        assert!(
            server_refs(&[ArtistRef {
                name: "Alpha".into(),
                id: None
            }])
            .is_empty()
        );

        let real = ArtistRef {
            name: "Beta".into(),
            id: Some("ar2".into()),
        };
        let kept = server_refs(&[real.clone(), stand_in]);
        assert_eq!(kept[0], real);
        assert_eq!(kept[1].id, None);
    }

    fn seed_entry(id: &str, album: &str, album_id: Option<&str>, bytes: u64) -> CachedTrack {
        CachedTrack {
            id: id.into(),
            name: format!("Song {id}"),
            artists: "Alpha".into(),
            album: album.into(),
            duration_ms: 1000,
            bytes,
            saved_at: 1,
            file: format!("{id}.audio"),
            album_id: album_id.map(str::to_owned),
            cover_url: None,
            artist_refs: Vec::new(),
            track_number: 1,
            disc_number: 1,
        }
    }

    /// Offline filesystem tests share process-wide TEST_ROOT / held state, so only one
    /// runs at a time.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_cache(f: impl FnOnce(&Path)) {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        let key = "test-account";
        {
            let mut guard = TEST_ROOT.lock().expect("root lock");
            *guard = Some(root.path().to_path_buf());
        }
        {
            let mut guard = TEST_KEY.lock().expect("key lock");
            *guard = Some(key.into());
        }
        // Reset the in-memory index so it loads from this temp root.
        {
            let mut held = held().lock().expect("held");
            held.key.clear();
            held.manifest = Manifest {
                version: 1,
                server: key.into(),
                tracks: Default::default(),
            };
        }
        f(root.path());
        {
            let mut guard = TEST_ROOT.lock().expect("root lock");
            *guard = None;
        }
        {
            let mut guard = TEST_KEY.lock().expect("key lock");
            *guard = None;
        }
        {
            let mut held = held().lock().expect("held");
            held.key.clear();
            held.manifest = Manifest {
                version: 1,
                ..Manifest::default()
            };
        }
    }

    fn tiny_jpeg() -> Vec<u8> {
        // 2x2 red JPEG
        let mut rgb = image::RgbImage::new(2, 2);
        for pixel in rgb.pixels_mut() {
            *pixel = image::Rgb([255, 0, 0]);
        }
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        encoder
            .encode(rgb.as_raw(), 2, 2, image::ExtendedColorType::Rgb8)
            .expect("encode");
        out
    }

    #[test]
    fn format_bytes_uses_sensible_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(format_bytes((3.2 * 1024.0 * 1024.0 * 1024.0) as u64), "3.2 GB");
    }

    #[test]
    fn cover_key_prefers_album_id() {
        assert_eq!(cover_key(Some("al1"), "t1"), "al1");
        assert_eq!(cover_key(None, "t1"), "t1");
        assert_eq!(cover_key(Some("  "), "t1"), "t1");
    }

    #[test]
    fn usage_counts_audio_covers_and_lyrics() {
        with_cache(|root| {
            let dir = root_for("test-account");
            fs::create_dir_all(covers_dir(&dir)).unwrap();
            let mut manifest = Manifest {
                version: 1,
                server: "test-account".into(),
                tracks: Default::default(),
            };
            let a = seed_entry("t1", "Record", Some("al1"), 100);
            let b = seed_entry("t2", "Record", Some("al1"), 200);
            fs::write(dir.join(&a.file), vec![0u8; 100]).unwrap();
            fs::write(dir.join(&b.file), vec![0u8; 200]).unwrap();
            fs::write(sidecar_in(&dir, "t1", LYRICS_EXT), b"{}").unwrap();
            write_cover_bytes(&cover_path_in(&dir, "al1"), &tiny_jpeg()).unwrap();
            manifest.tracks.insert("t1".into(), a);
            manifest.tracks.insert("t2".into(), b);
            persist("test-account", &manifest).unwrap();
            {
                let mut held = held().lock().unwrap();
                held.key = "test-account".into();
                held.manifest = manifest;
            }

            let used = usage();
            assert_eq!(used.tracks, 2);
            assert_eq!(used.audio_bytes, 300);
            assert!(used.lyrics_bytes > 0);
            assert!(used.cover_bytes > 0);
            assert_eq!(
                used.bytes,
                used.audio_bytes + used.cover_bytes + used.lyrics_bytes
            );

            let albums = albums();
            assert_eq!(albums.len(), 1);
            assert_eq!(albums[0].track_count, 2);
            assert_eq!(albums[0].album_id.as_deref(), Some("al1"));
            assert!(albums[0].bytes >= 300);
            assert!(albums[0].cover.is_some());
            let _ = root;
        });
    }

    #[test]
    fn per_album_cover_is_shared_and_dropped_with_last_track() {
        with_cache(|_| {
            let dir = root_for("test-account");
            fs::create_dir_all(&dir).unwrap();
            let jpeg = tiny_jpeg();
            keep_cover(&dir, "t1", Some("al1"), &jpeg);
            keep_cover(&dir, "t2", Some("al1"), &jpeg);
            let cover = cover_path_in(&dir, "al1");
            assert!(cover.is_file());
            // Second keep must not rewrite / duplicate.
            let size = file_size(&cover);
            keep_cover(&dir, "t3", Some("al1"), &jpeg);
            assert_eq!(file_size(&cover), size);

            let mut manifest = Manifest {
                version: 1,
                server: "test-account".into(),
                tracks: Default::default(),
            };
            manifest
                .tracks
                .insert("t1".into(), seed_entry("t1", "Record", Some("al1"), 1));
            manifest
                .tracks
                .insert("t2".into(), seed_entry("t2", "Record", Some("al1"), 1));
            fs::write(dir.join("t1.audio"), b"a").unwrap();
            fs::write(dir.join("t2.audio"), b"b").unwrap();
            persist("test-account", &manifest).unwrap();
            {
                let mut held = held().lock().unwrap();
                held.key = "test-account".into();
                held.manifest = manifest;
            }

            remove("t1").unwrap();
            assert!(cover.is_file(), "cover stays while another track uses it");
            assert!(cover_url_in(&dir, "t2", Some("al1")).is_some());

            remove("t2").unwrap();
            assert!(!cover.is_file(), "cover goes with the last track");
        });
    }

    #[test]
    fn legacy_per_track_covers_migrate_into_album_file() {
        with_cache(|_| {
            let dir = root_for("test-account");
            fs::create_dir_all(&dir).unwrap();
            let jpeg = tiny_jpeg();
            // Two tracks of the same album each have a legacy sidecar.
            fs::write(legacy_cover_in(&dir, "t1"), &jpeg).unwrap();
            fs::write(legacy_cover_in(&dir, "t2"), &jpeg).unwrap();
            fs::write(dir.join("t1.audio"), b"a").unwrap();
            fs::write(dir.join("t2.audio"), b"b").unwrap();

            let mut manifest = Manifest {
                version: 1,
                server: "test-account".into(),
                tracks: Default::default(),
            };
            manifest
                .tracks
                .insert("t1".into(), seed_entry("t1", "Record", Some("al1"), 1));
            manifest
                .tracks
                .insert("t2".into(), seed_entry("t2", "Record", Some("al1"), 1));
            validate("test-account", &mut manifest);
            persist("test-account", &manifest).unwrap();

            let album_cover = cover_path_in(&dir, "al1");
            assert!(album_cover.is_file());
            assert!(!legacy_cover_in(&dir, "t1").exists());
            assert!(!legacy_cover_in(&dir, "t2").exists());
            assert!(cover_url_in(&dir, "t1", Some("al1")).is_some());
            assert!(cover_url_in(&dir, "t2", Some("al1")).is_some());
        });
    }

    #[test]
    fn clear_by_album_removes_its_tracks_and_cover() {
        with_cache(|_| {
            let dir = root_for("test-account");
            fs::create_dir_all(&dir).unwrap();
            let jpeg = tiny_jpeg();
            keep_cover(&dir, "t1", Some("al1"), &jpeg);
            keep_cover(&dir, "other", Some("al2"), &jpeg);
            fs::write(dir.join("t1.audio"), b"a").unwrap();
            fs::write(dir.join("t2.audio"), b"b").unwrap();
            fs::write(dir.join("t3.audio"), b"c").unwrap();

            let mut manifest = Manifest {
                version: 1,
                server: "test-account".into(),
                tracks: Default::default(),
            };
            for (id, album, al) in [
                ("t1", "Record", Some("al1")),
                ("t2", "Record", Some("al1")),
                ("t3", "Other", Some("al2")),
            ] {
                manifest
                    .tracks
                    .insert(id.into(), seed_entry(id, album, al, 1));
            }
            persist("test-account", &manifest).unwrap();
            {
                let mut held = held().lock().unwrap();
                held.key = "test-account".into();
                held.manifest = manifest;
            }

            let doomed = tracks_for_album(Some("al1"), "Record");
            assert_eq!(doomed.len(), 2);
            let freed = size_of(&doomed);
            assert!(freed > 0);
            for id in &doomed {
                remove(id).unwrap();
            }
            assert!(!cover_path_in(&dir, "al1").is_file());
            assert!(cover_path_in(&dir, "al2").is_file());
            assert_eq!(list().len(), 1);
            assert_eq!(list()[0].id, "t3");
        });
    }
}
