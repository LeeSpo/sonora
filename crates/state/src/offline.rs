//! Explicit offline audio downloads for Subsonic / Navidrome.
//!
//! Tracks the listener has saved play from disk even when the server is unreachable. This is
//! not the metadata cache in `storage::Cache`; it is the audio itself.

use std::collections::{HashMap, HashSet};

use gpui::{App, Context, Entity, Task};
use music::Track;

use crate::{Io, Outcome, Toasts, join};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfflineStatus {
    Ready,
    Saving,
    Failed,
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
    /// Tracks currently on disk, keyed by track id.
    ready: HashMap<String, OfflineEntry>,
    /// In-flight downloads.
    saving: HashSet<String>,
    tasks: HashMap<String, Task<()>>,
}

impl Offline {
    pub fn new(io: Io) -> Self {
        let mut ready = HashMap::new();
        for cached in music::offline::list() {
            ready.insert(
                cached.id.clone(),
                OfflineEntry {
                    track: Track {
                        id: Some(cached.id.clone()),
                        name: cached.name,
                        playable: true,
                        artists: cached.artists,
                        artist_refs: Vec::new(),
                        album: cached.album,
                        album_id: None,
                        cover: None,
                        duration: std::time::Duration::from_millis(cached.duration_ms),
                        added_at: None,
                        added_by: None,
                        playcount: None,
                        popularity: 0,
                        explicit: false,
                        track_number: 0,
                        disc_number: 0,
                        tags: Vec::new(),
                        languages: Vec::new(),
                        credits: Vec::new(),
                    },
                    status: OfflineStatus::Ready,
                    bytes: cached.bytes,
                    error: None,
                },
            );
        }
        Self {
            io,
            ready,
            saving: HashSet::new(),
            tasks: HashMap::new(),
        }
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

    pub fn entries(&self) -> impl Iterator<Item = &OfflineEntry> {
        self.ready.values()
    }

    /// Save one or more Subsonic tracks for offline playback.
    pub fn save_tracks(&mut self, tracks: Vec<Track>, cx: &mut Context<Self>) {
        for track in tracks {
            let Some(id) = track.id.clone() else {
                continue;
            };
            if music::is_local_id(&id) {
                continue;
            }
            if self.is_saved(&id) || self.saving.contains(&id) {
                continue;
            }
            self.saving.insert(id.clone());
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
            let task = cx.spawn(async move |this, cx| {
                let result = join(io.spawn(async move { music::offline::save(&held).await })).await;
                this.update(cx, |this, cx| {
                    this.saving.remove(&task_id);
                    this.tasks.remove(&task_id);
                    match result {
                        Ok(cached) => {
                            if let Some(entry) = this.ready.get_mut(&task_id) {
                                entry.status = OfflineStatus::Ready;
                                entry.bytes = cached.bytes;
                                entry.error = None;
                            }
                            Toasts::show(Outcome::Done, "toast-offline-saved", cx);
                        }
                        Err(error) => {
                            log::warn!("offline: cannot save {task_id}: {error:#}");
                            if let Some(entry) = this.ready.get_mut(&task_id) {
                                entry.status = OfflineStatus::Failed;
                                entry.error = Some(format!("{error:#}"));
                            } else {
                                this.ready.remove(&task_id);
                            }
                            Toasts::show(Outcome::Failed, "toast-offline-failed", cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
            self.tasks.insert(id, task);
            cx.notify();
        }
    }

    pub fn remove_tracks(&mut self, track_ids: Vec<String>, cx: &mut Context<Self>) {
        for id in track_ids {
            if let Err(error) = music::offline::remove(&id) {
                log::warn!("offline: cannot remove {id}: {error:#}");
                Toasts::show(Outcome::Failed, "toast-offline-remove-failed", cx);
                continue;
            }
            self.ready.remove(&id);
            self.saving.remove(&id);
            self.tasks.remove(&id);
        }
        Toasts::show(Outcome::Done, "toast-offline-removed", cx);
        cx.notify();
    }
}
