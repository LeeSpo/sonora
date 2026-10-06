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
/// Sidecar with the cover image kept for a saved track (`{id}.cover`).
const COVER_EXT: &str = "cover";
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

    let known: HashSet<String> = manifest
        .tracks
        .values()
        .map(|entry| entry.file.clone())
        .collect();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == MANIFEST || name.ends_with(".json.tmp") {
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
    if let Some(url) = track.cover.as_deref().filter(|url| url.starts_with("http")) {
        match client.fetch_bytes(url).await {
            Ok(bytes) if !bytes.is_empty() => {
                if let Err(error) = fs::write(sidecar_in(&dir, id, COVER_EXT), &bytes) {
                    log::debug!("offline: cannot keep the cover for {id}: {error}");
                }
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

/// Cover image kept beside a saved track, as a `file://` url the artwork loader reads.
pub fn cover(track_id: &str) -> Option<String> {
    let path = sidecar_in(&root()?, track_id, COVER_EXT);
    let meta = fs::metadata(&path).ok()?;
    (meta.is_file() && meta.len() > 0).then(|| format!("file://{}", path.display()))
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
}
