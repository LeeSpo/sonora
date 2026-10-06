//! Explicit offline audio downloads for Subsonic / Navidrome.
//!
//! Tracks the listener has saved play from disk even when the server is unreachable. This is
//! not the metadata cache in `storage::Cache`; it is the audio itself.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, Context, Entity, Task};
use music::{ArtistRef, Track};
use tokio::task::AbortHandle;

use crate::network::{Network, Reconnected};
use crate::session::{Session, SessionEvent};
use crate::{Io, Outcome, Playback, Queue, Toasts, join};

/// How many saved tracks one backfill request asks the server about at once.
const BACKFILL_BATCH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfflineStatus {
    Ready,
    Saving,
    Failed,
}

/// One artist's saved tracks, built only from the offline index so it reads with no network.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineArtist {
    /// The artist id the page was opened with: a server id, or a stand-in name id.
    pub key: String,
    pub name: String,
    /// Saved albums of the artist, by name.
    pub albums: Vec<OfflineAlbum>,
    /// Every saved track of the artist, album by album in track order, then the loose ones.
    pub tracks: Vec<Track>,
}

/// One album among an artist's saved tracks, with only the tracks that are on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineAlbum {
    pub id: Option<String>,
    pub name: String,
    pub cover: Option<String>,
    pub tracks: Vec<Track>,
    /// Audio + lyrics + this album's cover, when known.
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct OfflineEntry {
    pub track: Track,
    pub status: OfflineStatus,
    pub bytes: u64,
    pub error: Option<String>,
}

pub struct Offline {
    io: Io,
    session: Entity<Session>,
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    /// The artist the offline artist page shows, rebuilt whenever the saved tracks change.
    artist: Option<OfflineArtist>,
    /// The key the offline artist page was opened with.
    artist_key: Option<String>,
    /// Looks up artist ids for rows saved before they were kept. One run at a time.
    backfill: Option<Task<()>>,
    /// Ids the server already answered for this session, so a song with no artist id on the
    /// server is not asked about again on every reconnect.
    asked: HashSet<String>,
    /// The last track the player reported, so a play only updates its row once.
    last_played: Option<String>,
    /// Tracks currently on disk, keyed by track id.
    ready: HashMap<String, OfflineEntry>,
    /// Ready tracks in save order (newest first), for the Downloaded screen.
    tracks: Vec<Track>,
    /// Saving and failed tracks in queue order, shown above `tracks` on the Downloaded screen.
    pending: Vec<Track>,
    /// `pending` followed by `tracks`: what the Downloaded screen lists.
    listed: Vec<Track>,
    /// In-flight downloads.
    saving: HashSet<String>,
    /// Saves queued since the queue was last empty; with `saving` this gives "x of y" progress.
    queued: usize,
    tasks: HashMap<String, Task<()>>,
    /// Aborts the nested tokio download when the listener removes a saving track.
    aborts: HashMap<String, AbortHandle>,
}

impl Offline {
    pub fn new(
        session: Entity<Session>,
        network: Entity<Network>,
        playback: Entity<Playback>,
        queue: Entity<Queue>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn | SessionEvent::SignedOut => {
                this.asked.clear();
                this.refresh(cx);
            }
            SessionEvent::Reconnected => this.refresh(cx),
            SessionEvent::LocalChanged => {}
        })
        .detach();
        cx.subscribe(&network, |this, _, _: &Reconnected, cx| this.backfill(cx))
            .detach();
        cx.observe(&playback, |this, playback, cx| {
            let track = playback.read(cx).track().cloned();
            this.played(track, cx);
        })
        .detach();

        let mut this = Self {
            io,
            session,
            playback,
            queue,
            artist: None,
            artist_key: None,
            backfill: None,
            asked: HashSet::new(),
            last_played: None,
            ready: HashMap::new(),
            tracks: Vec::new(),
            pending: Vec::new(),
            listed: Vec::new(),
            saving: HashSet::new(),
            queued: 0,
            tasks: HashMap::new(),
            aborts: HashMap::new(),
        };
        this.reload_ready();
        this.backfill(cx);
        this
    }

    /// Drop in-flight saves that belong to another account and rebuild `ready` from disk for
    /// the signed-in Subsonic account (or clear it after logout).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        music::offline::reload();
        // Abort downloads: their account may no longer be the active one.
        for (_, abort) in self.aborts.drain() {
            abort.abort();
        }
        self.tasks.clear();
        self.saving.clear();
        self.queued = 0;
        self.pending.clear();
        self.backfill = None;
        self.reload_ready();
        self.backfill(cx);
        cx.notify();
    }

    fn reload_ready(&mut self) {
        self.ready.clear();
        self.tracks.clear();
        for cached in music::offline::list() {
            let track = track_from_cached(&cached);
            self.ready.insert(
                cached.id.clone(),
                OfflineEntry {
                    track: track.clone(),
                    status: OfflineStatus::Ready,
                    bytes: cached.bytes,
                    error: None,
                },
            );
            self.tracks.push(track);
        }
        self.relist();
    }

    fn relist(&mut self) {
        self.listed.clear();
        self.listed.extend(self.pending.iter().cloned());
        self.listed.extend(self.tracks.iter().cloned());
        self.artist = self
            .artist_key
            .as_deref()
            .and_then(|key| artist_tracks(key, &self.tracks));
    }

    /// Point the offline artist page at `key` (a server artist id or a stand-in name id).
    pub fn open_artist(&mut self, key: &str, cx: &mut Context<Self>) {
        self.artist_key = Some(key.to_owned());
        self.artist = artist_tracks(key, &self.tracks);
        cx.notify();
    }

    /// The artist the offline artist page shows, or `None` when nothing of it is saved.
    pub fn artist(&self) -> Option<&OfflineArtist> {
        self.artist.as_ref()
    }

    /// The saved tracks of the artist the offline artist page shows.
    pub fn artist_listed(&self) -> &[Track] {
        self.artist
            .as_ref()
            .map(|artist| artist.tracks.as_slice())
            .unwrap_or_default()
    }

    /// Whether anything credited to `key` is saved, so the artist link can open the offline
    /// artist page rather than a page that cannot load.
    pub fn has_artist(&self, key: &str) -> bool {
        let name = artist_name(key, &self.tracks);
        self.tracks
            .iter()
            .any(|track| credits(track, key, name.as_deref()))
    }

    /// Look up artist ids for saved rows that predate them, a small batch at a time, while the
    /// server can be reached. Each batch is written to the index as it lands, so the artist
    /// links of those tracks work from then on, offline too.
    pub fn backfill(&mut self, cx: &mut Context<Self>) {
        if self.backfill.is_some() || Network::lost(cx) {
            return;
        }
        if self.session.read(cx).provider_slug() != Some("subsonic") {
            return;
        }
        let wanted: Vec<String> = music::offline::missing_artists()
            .into_iter()
            .filter(|id| !self.asked.contains(id))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let io = self.io.clone();
        self.backfill = Some(cx.spawn(async move |this, cx| {
            for batch in wanted.chunks(BACKFILL_BATCH) {
                let ids = batch.to_vec();
                let found =
                    join(io.spawn(async move { music::offline::backfill(ids).await })).await;
                let more = this
                    .update(cx, |this, cx| match found {
                        Ok(found) => {
                            this.asked.extend(found.answered);
                            this.learned(found.changed, cx);
                            true
                        }
                        Err(error) => {
                            log::info!("offline: artist lookup paused: {error:#}");
                            false
                        }
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |this, _| this.backfill = None).ok();
        }));
    }

    /// Note a track the player started: a saved track that came with artist ids hands them
    /// to its row in the index, so its link works offline next time.
    fn played(&mut self, track: Option<Track>, cx: &mut Context<Self>) {
        let Some(track) = track else {
            return;
        };
        let Some(id) = track.id.clone() else {
            return;
        };
        if self.last_played.as_deref() == Some(id.as_str()) {
            return;
        }
        self.last_played = Some(id.clone());
        let stale = self.ready.get(&id).is_some_and(|entry| {
            entry.status == OfflineStatus::Ready && !linked(&entry.track.artist_refs)
        });
        if !stale || !linked(&track.artist_refs) {
            return;
        }
        let io = self.io.clone();
        let write = io.spawn_blocking(move || music::offline::remember(&[track]));
        cx.spawn(async move |this, cx| {
            let Ok(changed) = write.await else {
                return;
            };
            this.update(cx, |this, cx| this.learned(changed, cx)).ok();
        })
        .detach();
    }

    /// Apply rows whose artist ids were just recorded: the Downloaded list, the offline artist
    /// page, and the same tracks in the player and the queue all pick up the links.
    fn learned(&mut self, changed: Vec<music::offline::CachedTrack>, cx: &mut Context<Self>) {
        if changed.is_empty() {
            return;
        }
        for cached in &changed {
            let fresh = track_from_cached(cached);
            if let Some(entry) = self.ready.get_mut(&cached.id) {
                entry.track.artist_refs = fresh.artist_refs.clone();
                entry.track.track_number = fresh.track_number;
                entry.track.disc_number = fresh.disc_number;
            }
            for track in self
                .tracks
                .iter_mut()
                .filter(|track| track.id.as_deref() == Some(cached.id.as_str()))
            {
                track.artist_refs = fresh.artist_refs.clone();
                track.track_number = fresh.track_number;
                track.disc_number = fresh.disc_number;
            }
            let refs = fresh.artist_refs;
            let id = cached.id.clone();
            self.playback.update(cx, |playback, cx| {
                playback.amend_artists(&id, &refs, cx);
            });
            self.queue
                .update(cx, |queue, cx| queue.amend_artists(&id, &refs, cx));
        }
        self.relist();
        cx.notify();
    }

    pub fn global(cx: &App) -> Entity<Self> {
        crate::Sonora::global(cx).offline.clone()
    }

    pub fn is_saved(&self, track_id: &str) -> bool {
        self.ready
            .get(track_id)
            .is_some_and(|entry| entry.status == OfflineStatus::Ready)
            || music::offline::is_cached(track_id)
    }

    pub fn is_saving(&self, track_id: &str) -> bool {
        self.saving.contains(track_id)
    }

    pub fn is_failed(&self, track_id: &str) -> bool {
        !self.saving.contains(track_id)
            && self
                .ready
                .get(track_id)
                .is_some_and(|entry| entry.status == OfflineStatus::Failed)
    }

    /// What a song row should show: saved, saving, failed, or nothing.
    pub fn status(&self, track_id: &str) -> Option<OfflineStatus> {
        if self.saving.contains(track_id) {
            Some(OfflineStatus::Saving)
        } else if self.is_failed(track_id) {
            Some(OfflineStatus::Failed)
        } else if self.is_saved(track_id) {
            Some(OfflineStatus::Ready)
        } else {
            None
        }
    }

    /// `(settled, queued)` for the saves running now, or `None` when nothing is saving.
    pub fn progress(&self) -> Option<(usize, usize)> {
        let active = self.saving.len();
        (active > 0).then(|| {
            let total = self.queued.max(active);
            (total - active, total)
        })
    }

    pub fn failed_count(&self) -> usize {
        self.pending
            .iter()
            .filter_map(|track| track.id.as_deref())
            .filter(|id| self.is_failed(id))
            .count()
    }

    /// Forget failed saves without retrying; `None` dismisses every failed track.
    pub fn dismiss_failed(&mut self, track_ids: Option<Vec<String>>, cx: &mut Context<Self>) {
        let doomed: Vec<String> = self
            .pending
            .iter()
            .filter_map(|track| track.id.clone())
            .filter(|id| {
                self.is_failed(id)
                    && track_ids
                        .as_ref()
                        .is_none_or(|wanted| wanted.iter().any(|held| held == id))
            })
            .collect();
        if doomed.is_empty() {
            return;
        }
        for id in &doomed {
            self.ready.remove(id);
            self.drop_pending(id);
        }
        self.relist();
        cx.notify();
    }

    /// Re-queue failed saves; `None` retries every failed track.
    pub fn retry(&mut self, track_ids: Option<Vec<String>>, cx: &mut Context<Self>) {
        let tracks: Vec<Track> = self
            .pending
            .iter()
            .filter(|track| {
                track.id.as_deref().is_some_and(|id| {
                    self.is_failed(id)
                        && track_ids
                            .as_ref()
                            .is_none_or(|wanted| wanted.iter().any(|held| held == id))
                })
            })
            .cloned()
            .collect();
        self.save_tracks(tracks, cx);
    }

    pub fn entries(&self) -> impl Iterator<Item = &OfflineEntry> {
        self.ready.values()
    }

    /// Ready offline tracks, newest first.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Saving and failed tracks first, then ready ones — used by the Downloaded screen.
    pub fn listed(&self) -> &[Track] {
        &self.listed
    }

    /// How much space ready offline saves take on disk.
    pub fn usage(&self) -> music::offline::Usage {
        music::offline::usage()
    }

    /// Saved albums with song count and bytes, for the Downloaded screen.
    pub fn albums(&self) -> Vec<music::offline::AlbumGroup> {
        music::offline::albums()
    }

    /// Bytes the given ready track ids take (for confirmations).
    pub fn size_of(&self, track_ids: &[String]) -> u64 {
        music::offline::size_of(track_ids)
    }

    pub fn is_empty(&self) -> bool {
        self.listed.is_empty()
    }

    fn hold_pending(&mut self, track: &Track) {
        let id = track.id.as_deref();
        if !self.pending.iter().any(|held| held.id.as_deref() == id) {
            self.pending.push(track.clone());
        }
    }

    fn drop_pending(&mut self, track_id: &str) {
        self.pending
            .retain(|track| track.id.as_deref() != Some(track_id));
    }

    fn remember_ready(&mut self, track: Track) {
        let Some(id) = track.id.clone() else {
            return;
        };
        self.tracks
            .retain(|held| held.id.as_deref() != Some(id.as_str()));
        self.tracks.insert(0, track);
    }

    fn forget_ready(&mut self, track_id: &str) {
        self.tracks
            .retain(|track| track.id.as_deref() != Some(track_id));
    }

    /// Save one or more Subsonic tracks for offline playback.
    pub fn save_tracks(&mut self, tracks: Vec<Track>, cx: &mut Context<Self>) {
        if crate::Sonora::global(cx).session.read(cx).provider_slug() != Some("subsonic") {
            log::warn!("offline: save is only available on Subsonic");
            return;
        }
        let batch: Vec<Track> = tracks
            .into_iter()
            .filter(|track| {
                track.id.as_deref().is_some_and(|id| {
                    !music::is_local_id(id) && !self.is_saved(id) && !self.saving.contains(id)
                })
            })
            .collect();
        if batch.is_empty() {
            return;
        }
        if self.saving.is_empty() {
            self.queued = 0;
        }
        self.queued += batch.len();

        let remaining = Arc::new(AtomicUsize::new(batch.len()));
        let succeeded = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));

        for track in batch {
            let Some(id) = track.id.clone() else {
                continue;
            };
            self.saving.insert(id.clone());
            self.hold_pending(&track);
            self.ready.insert(
                id.clone(),
                OfflineEntry {
                    track: track.clone(),
                    status: OfflineStatus::Saving,
                    bytes: 0,
                    error: None,
                },
            );
            let io = self.io.clone();
            let held = track.clone();
            let task_id = id.clone();
            let remaining = remaining.clone();
            let succeeded = succeeded.clone();
            let failed = failed.clone();
            let download = io.spawn(async move { music::offline::save(&held).await });
            self.aborts.insert(id.clone(), download.abort_handle());
            let task = cx.spawn(async move |this, cx| {
                let result = join(download).await;
                this.update(cx, |this, cx| {
                    this.saving.remove(&task_id);
                    this.tasks.remove(&task_id);
                    this.aborts.remove(&task_id);
                    match result {
                        Ok(cached) => {
                            let cover = music::offline::cover(&task_id);
                            let ready_track = this.ready.get_mut(&task_id).map(|entry| {
                                entry.status = OfflineStatus::Ready;
                                entry.bytes = cached.bytes;
                                entry.error = None;
                                // Prefer the kept image so the row keeps its art offline.
                                if cover.is_some() {
                                    entry.track.cover = cover.clone();
                                }
                                entry.track.clone()
                            });
                            this.drop_pending(&task_id);
                            if let Some(track) = ready_track {
                                this.remember_ready(track);
                            }
                            succeeded.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(error) => {
                            let cancelled = format!("{error:#}").contains("cancelled")
                                || format!("{error:#}").contains("canceled");
                            if cancelled {
                                this.ready.remove(&task_id);
                                this.drop_pending(&task_id);
                            } else {
                                log::warn!("offline: cannot save {task_id}: {error:#}");
                                if let Some(entry) = this.ready.get_mut(&task_id) {
                                    entry.status = OfflineStatus::Failed;
                                    entry.error = Some(format!("{error:#}"));
                                } else {
                                    this.ready.remove(&task_id);
                                    this.drop_pending(&task_id);
                                }
                                failed.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    // One toast for the whole batch, once the last save settles.
                    if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                        let fails = failed.load(Ordering::SeqCst);
                        let oks = succeeded.load(Ordering::SeqCst);
                        if oks > 0 || fails > 0 {
                            let outcome = if fails > 0 {
                                Outcome::Failed
                            } else {
                                Outcome::Done
                            };
                            Toasts::show_counts(
                                outcome,
                                "toast-offline-save-summary",
                                oks,
                                fails,
                                cx,
                            );
                        }
                    }
                    if this.saving.is_empty() {
                        this.queued = 0;
                    }
                    this.relist();
                    cx.notify();
                })
                .ok();
            });
            self.tasks.insert(id, task);
        }
        self.relist();
        cx.notify();
    }

    pub fn remove_tracks(&mut self, track_ids: Vec<String>, cx: &mut Context<Self>) {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for id in track_ids {
            // Abort the nested tokio download before (or while) removing files so a late
            // write cannot recreate the track after remove.
            if let Some(abort) = self.aborts.remove(&id) {
                abort.abort();
            }
            self.tasks.remove(&id);
            match music::offline::remove(&id) {
                Ok(()) => {
                    self.ready.remove(&id);
                    self.saving.remove(&id);
                    self.drop_pending(&id);
                    self.forget_ready(&id);
                    succeeded += 1;
                }
                Err(error) => {
                    log::warn!("offline: cannot remove {id}: {error:#}");
                    failed += 1;
                }
            }
        }
        if succeeded > 0 || failed > 0 {
            let outcome = if failed > 0 {
                Outcome::Failed
            } else {
                Outcome::Done
            };
            Toasts::show_counts(
                outcome,
                "toast-offline-remove-summary",
                succeeded,
                failed,
                cx,
            );
        }
        if self.saving.is_empty() {
            self.queued = 0;
        }
        self.relist();
        cx.notify();
    }
}

fn track_from_cached(cached: &music::offline::CachedTrack) -> Track {
    Track {
        id: Some(cached.id.clone()),
        name: cached.name.clone(),
        playable: true,
        artists: cached.artists.clone(),
        artist_refs: music::offline::credited(cached),
        album: cached.album.clone(),
        album_id: cached.album_id.clone(),
        cover: music::offline::cover(&cached.id).or_else(|| cached.cover_url.clone()),
        duration: std::time::Duration::from_millis(cached.duration_ms),
        added_at: None,
        added_by: None,
        playcount: None,
        popularity: 0,
        explicit: false,
        track_number: cached.track_number,
        disc_number: cached.disc_number,
        tags: Vec::new(),
        languages: Vec::new(),
        credits: Vec::new(),
    }
}

/// Whether `refs` hold at least one server artist id, rather than none or only stand-ins.
pub(crate) fn linked(refs: &[ArtistRef]) -> bool {
    refs.iter().any(|artist| {
        artist
            .id
            .as_deref()
            .is_some_and(|id| !music::offline::is_name_artist_id(id))
    })
}

/// The name of the artist `key` stands for: the name inside a stand-in id, or the name a saved
/// track credits next to that server id.
fn artist_name(key: &str, tracks: &[Track]) -> Option<String> {
    if let Some(name) = music::offline::name_of_artist_id(key) {
        return Some(name.to_owned());
    }
    tracks
        .iter()
        .flat_map(|track| track.artist_refs.iter())
        .find(|artist| artist.id.as_deref() == Some(key))
        .map(|artist| artist.name.clone())
}

/// Whether `track` is credited to the artist `key`. A server id matches its own id, and also
/// a same-named artist on a row that has no server id yet; a stand-in id matches by name.
fn credits(track: &Track, key: &str, name: Option<&str>) -> bool {
    let stand_in = music::offline::is_name_artist_id(key);
    track.artist_refs.iter().any(|artist| {
        if artist.id.as_deref() == Some(key) {
            return true;
        }
        let Some(name) = name else {
            return false;
        };
        let unlinked = artist
            .id
            .as_deref()
            .is_none_or(music::offline::is_name_artist_id);
        (stand_in || unlinked) && music::offline::same_name(&artist.name, name)
    })
}

/// Everything saved of the artist `key`, grouped into albums. `None` when nothing is saved,
/// so the offline artist page is never empty.
pub(crate) fn artist_tracks(key: &str, tracks: &[Track]) -> Option<OfflineArtist> {
    let name = artist_name(key, tracks);
    let mine: Vec<&Track> = tracks
        .iter()
        .filter(|track| credits(track, key, name.as_deref()))
        .collect();
    if mine.is_empty() {
        return None;
    }

    let mut albums: Vec<OfflineAlbum> = Vec::new();
    let mut loose: Vec<Track> = Vec::new();
    for track in mine {
        if track.album.trim().is_empty() {
            loose.push(track.clone());
            continue;
        }
        let same = |album: &&mut OfflineAlbum| match (&album.id, &track.album_id) {
            (Some(held), Some(id)) => held == id,
            _ => music::offline::same_name(&album.name, &track.album),
        };
        match albums.iter_mut().find(|album| same(album)) {
            Some(album) => {
                if album.cover.is_none() {
                    album.cover = track.cover.clone();
                }
                album.tracks.push(track.clone());
            }
            None => albums.push(OfflineAlbum {
                id: track.album_id.clone(),
                name: track.album.clone(),
                cover: track.cover.clone(),
                tracks: vec![track.clone()],
                bytes: 0,
            }),
        }
    }
    albums.sort_by_key(|album| album.name.to_lowercase());
    for album in &mut albums {
        album.tracks.sort_by_key(|track| {
            (
                track.disc_number,
                track.track_number,
                track.name.to_lowercase(),
            )
        });
    }
    loose.sort_by_key(|track| track.name.to_lowercase());

    let display = name.unwrap_or_else(|| {
        albums
            .iter()
            .flat_map(|album| album.tracks.iter())
            .chain(loose.iter())
            .find_map(|track| track.artist_refs.first().map(|artist| artist.name.clone()))
            .unwrap_or_default()
    });
    for album in &mut albums {
        let ids: Vec<String> = album
            .tracks
            .iter()
            .filter_map(|track| track.id.clone())
            .collect();
        album.bytes = music::offline::size_of(&ids);
    }

    let tracks = albums
        .iter()
        .flat_map(|album| album.tracks.iter().cloned())
        .chain(loose)
        .collect();
    Some(OfflineArtist {
        key: key.to_owned(),
        name: display,
        albums,
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(id: &str, artists: &str, album: &str, refs: Vec<ArtistRef>) -> Track {
        track_from_cached(&music::offline::CachedTrack {
            id: id.into(),
            name: format!("Song {id}"),
            artists: artists.into(),
            album: album.into(),
            duration_ms: 1000,
            bytes: 1,
            saved_at: 1,
            file: format!("{id}.audio"),
            album_id: (!album.is_empty()).then(|| format!("al-{album}")),
            cover_url: None,
            artist_refs: refs,
            track_number: 0,
            disc_number: 0,
        })
    }

    fn artist(name: &str, id: &str) -> ArtistRef {
        ArtistRef {
            name: name.into(),
            id: Some(id.into()),
        }
    }

    fn ids(tracks: &[Track]) -> Vec<&str> {
        tracks
            .iter()
            .filter_map(|track| track.id.as_deref())
            .collect()
    }

    #[test]
    fn an_old_row_links_each_artist_by_name() {
        let track = cached("t1", "Alpha, Beta", "Record", Vec::new());
        let names: Vec<_> = track.artist_refs.iter().map(|a| a.name.as_str()).collect();

        assert_eq!(names, ["Alpha", "Beta"]);
        assert!(!linked(&track.artist_refs));
        assert!(track.artist_refs.iter().all(|a| a.id.is_some()));
    }

    #[test]
    fn a_name_link_finds_old_and_new_rows_of_that_artist() {
        let tracks = vec![
            cached("old", "Alpha, Beta", "Record", Vec::new()),
            cached("new", "Alpha", "Other", vec![artist("Alpha", "ar1")]),
            cached("else", "Gamma", "Record", Vec::new()),
        ];
        let key = music::offline::name_artist_id("alpha");
        let found = artist_tracks(&key, &tracks).expect("alpha is saved");

        assert_eq!(ids(&found.tracks), ["new", "old"]);
        assert_eq!(found.name, "alpha");
    }

    #[test]
    fn a_server_id_also_finds_rows_that_only_have_the_name() {
        let tracks = vec![
            cached("old", "Alpha, Beta", "Record", Vec::new()),
            cached("new", "Alpha", "Other", vec![artist("Alpha", "ar1")]),
            cached("namesake", "Alpha", "Third", vec![artist("Alpha", "ar9")]),
        ];
        let found = artist_tracks("ar1", &tracks).expect("ar1 is saved");

        assert_eq!(ids(&found.tracks), ["new", "old"]);
        assert_eq!(found.name, "Alpha");
        assert_eq!(found.albums.len(), 2);
    }

    #[test]
    fn a_single_saved_song_still_makes_a_page() {
        let tracks = vec![cached("only", "Solo", "Lone", vec![artist("Solo", "ar5")])];
        let found = artist_tracks("ar5", &tracks).expect("one song is enough");

        assert_eq!(ids(&found.tracks), ["only"]);
        assert_eq!(found.albums.len(), 1);
        assert_eq!(ids(&found.albums[0].tracks), ["only"]);

        let loose = vec![cached("loose", "Solo", "", Vec::new())];
        let key = music::offline::name_artist_id("Solo");
        let found = artist_tracks(&key, &loose).expect("a song with no album still lists");
        assert!(found.albums.is_empty());
        assert_eq!(ids(&found.tracks), ["loose"]);
    }

    #[test]
    fn nothing_saved_means_no_page() {
        let tracks = vec![cached("t1", "Alpha", "Record", Vec::new())];

        assert!(artist_tracks("ar404", &tracks).is_none());
        assert!(artist_tracks(&music::offline::name_artist_id("Beta"), &tracks).is_none());
    }

    #[test]
    fn album_tracks_play_in_disc_and_track_order() {
        let mut second = cached("b", "Alpha", "Record", vec![artist("Alpha", "ar1")]);
        second.track_number = 2;
        let mut first = cached("a", "Alpha", "Record", vec![artist("Alpha", "ar1")]);
        first.track_number = 1;
        let mut later = cached("c", "Alpha", "Record", vec![artist("Alpha", "ar1")]);
        later.disc_number = 2;
        later.track_number = 1;
        let found = artist_tracks("ar1", &[later, second, first]).expect("saved");

        assert_eq!(ids(&found.tracks), ["a", "b", "c"]);
    }
}
