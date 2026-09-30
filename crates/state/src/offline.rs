//! Explicit offline audio downloads for Subsonic / Navidrome.
//!
//! Tracks the listener has saved play from disk even when the server is unreachable. This is
//! not the metadata cache in `storage::Cache`; it is the audio itself.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, Context, Entity, Task};
use music::Track;
use tokio::task::AbortHandle;

use crate::session::{Session, SessionEvent};
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
    /// Aborts the nested tokio download when the listener removes a saving track.
    aborts: HashMap<String, AbortHandle>,
}

impl Offline {
    pub fn new(session: Entity<Session>, io: Io, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn | SessionEvent::SignedOut | SessionEvent::Reconnected => {
                this.refresh(cx);
            }
            SessionEvent::LocalChanged => {}
        })
        .detach();

        let mut this = Self {
            io,
            ready: HashMap::new(),
            saving: HashSet::new(),
            tasks: HashMap::new(),
            aborts: HashMap::new(),
        };
        this.reload_ready();
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
        self.reload_ready();
        cx.notify();
    }

    fn reload_ready(&mut self) {
        self.ready.clear();
        for cached in music::offline::list() {
            self.ready.insert(
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
        if crate::Sonora::global(cx).session.read(cx).provider_slug() != Some("subsonic") {
            log::warn!("offline: save is only available on Subsonic");
            return;
        }
        let batch: Vec<Track> = tracks
            .into_iter()
            .filter(|track| {
                track
                    .id
                    .as_deref()
                    .is_some_and(|id| !music::is_local_id(id) && !self.is_saved(id) && !self.saving.contains(id))
            })
            .collect();
        if batch.is_empty() {
            return;
        }

        let remaining = Arc::new(AtomicUsize::new(batch.len()));
        let succeeded = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));

        for track in batch {
            let Some(id) = track.id.clone() else {
                continue;
            };
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
                            if let Some(entry) = this.ready.get_mut(&task_id) {
                                entry.status = OfflineStatus::Ready;
                                entry.bytes = cached.bytes;
                                entry.error = None;
                            }
                            succeeded.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(error) => {
                            let cancelled = format!("{error:#}").contains("cancelled")
                                || format!("{error:#}").contains("canceled");
                            if cancelled {
                                this.ready.remove(&task_id);
                            } else {
                                log::warn!("offline: cannot save {task_id}: {error:#}");
                                if let Some(entry) = this.ready.get_mut(&task_id) {
                                    entry.status = OfflineStatus::Failed;
                                    entry.error = Some(format!("{error:#}"));
                                } else {
                                    this.ready.remove(&task_id);
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
                    cx.notify();
                })
                .ok();
            });
            self.tasks.insert(id, task);
            cx.notify();
        }
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
        cx.notify();
    }
}
