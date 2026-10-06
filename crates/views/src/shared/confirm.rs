use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Global, Render, Window, div};
use i18n::t;
use music::{Album, SavedArtist, Shape, Track};
use state::{Detail, History, Io, Outcome, Shelf, Sonora, Toasts};
use ui::{Button, Dismiss, FORM_CONTEXT, Modal, Submit};

#[derive(Clone)]
pub(crate) enum Kind {
    LibrarySongs(usize),
    PlaylistSongs(usize),
    History(usize),
    Albums(usize),
    Artists(usize),
    Playlists(usize),
    DeleteTrackFiles(usize),
    Widevine,
    /// Remove several offline copies. `size` is already human-readable (e.g. "3.2 GB").
    OfflineTracks { count: usize, size: gpui::SharedString },
    /// Remove every offline copy of one album.
    OfflineAlbum {
        name: gpui::SharedString,
        count: usize,
        size: gpui::SharedString,
    },
    /// Remove every offline copy of one playlist's tracks.
    OfflinePlaylist {
        name: gpui::SharedString,
        count: usize,
        size: gpui::SharedString,
    },
    /// Wipe the whole offline cache.
    OfflineAll { count: usize, size: gpui::SharedString },
}

impl Kind {
    fn title(&self) -> gpui::SharedString {
        match self {
            Self::PlaylistSongs(_) => t!("confirm-remove-playlist-title"),
            Self::History(_) => t!("confirm-remove-history-title"),
            Self::DeleteTrackFiles(_) => t!("confirm-delete-track-files-title"),
            Self::Widevine => t!("confirm-uninstall-widevine-title"),
            Self::OfflineTracks { .. } | Self::OfflineAlbum { .. } | Self::OfflinePlaylist { .. } => {
                t!("confirm-offline-remove-title")
            }
            Self::OfflineAll { .. } => t!("confirm-offline-clear-all-title"),
            _ => t!("confirm-remove-library-title"),
        }
    }

    fn detail(&self) -> gpui::SharedString {
        match self {
            Self::LibrarySongs(count) => t!("confirm-remove-songs", count = *count),
            Self::PlaylistSongs(count) => t!("confirm-remove-playlist-songs", count = *count),
            Self::History(count) => t!("confirm-remove-history-songs", count = *count),
            Self::Albums(count) => t!("confirm-remove-albums", count = *count),
            Self::Artists(count) => t!("confirm-remove-artists", count = *count),
            Self::Playlists(count) => t!("confirm-remove-playlists", count = *count),
            Self::DeleteTrackFiles(count) => t!("confirm-delete-track-files", count = *count),
            Self::Widevine => t!("confirm-uninstall-widevine"),
            Self::OfflineTracks { count, size } => {
                t!("confirm-offline-remove-tracks", count = *count, size = size.as_ref())
            }
            Self::OfflineAlbum { name, count, size } => t!(
                "confirm-offline-remove-album",
                name = name.as_ref(),
                count = *count,
                size = size.as_ref()
            ),
            Self::OfflinePlaylist { name, count, size } => t!(
                "confirm-offline-remove-playlist",
                name = name.as_ref(),
                count = *count,
                size = size.as_ref()
            ),
            Self::OfflineAll { count, size } => {
                t!("confirm-offline-clear-all", count = *count, size = size.as_ref())
            }
        }
    }

    fn action(&self) -> gpui::SharedString {
        match self {
            Self::Widevine => t!("settings-widevine-uninstall"),
            Self::OfflineAll { .. } => t!("offline-clear-all"),
            _ => t!("common-delete"),
        }
    }
}

type Apply = Box<dyn FnOnce(&mut App)>;

struct Pending {
    kind: Kind,
    apply: Apply,
}

pub(crate) struct Confirm {
    pending: Option<Pending>,
    focus: FocusHandle,
    restore: Option<FocusHandle>,
    grab: bool,
}

struct Installed(Entity<Confirm>);

impl Global for Installed {}

impl Confirm {
    pub fn entity(cx: &mut App) -> Entity<Self> {
        if cx.try_global::<Installed>().is_none() {
            let confirm = cx.new(|cx| Self {
                pending: None,
                focus: cx.focus_handle(),
                restore: None,
                grab: false,
            });
            cx.set_global(Installed(confirm));
        }
        cx.global::<Installed>().0.clone()
    }

    /// Whether taking the heart off `id` only unstars it, so no question is worth asking. On a
    /// `Shape::Catalog` shelf the favorites sit over a library that stays put; on a
    /// `Shape::Saved` one the heart is the library itself, and the question stands.
    pub(crate) fn unstarring(id: &str, cx: &App) -> bool {
        Sonora::global(cx).library.read(cx).shape(Shelf::of(id)) == Shape::Catalog
    }

    pub fn ask(kind: Kind, apply: impl FnOnce(&mut App) + 'static, cx: &mut App) {
        let confirm = Self::entity(cx);
        confirm.update(cx, |this, cx| {
            this.pending = Some(Pending {
                kind,
                apply: Box::new(apply),
            });
            this.grab = true;
            cx.notify();
        });
    }

    pub fn library_songs(tracks: Vec<Track>, cx: &mut App) {
        if tracks.is_empty() {
            return;
        }
        let tracks_len = tracks.len();
        let starred = tracks
            .first()
            .and_then(|track| track.id.as_deref())
            .is_some_and(|id| Self::unstarring(id, cx));
        let apply = move |cx: &mut App| {
            let library = Sonora::global(cx).library.clone();
            library.update(cx, |library, cx| library.save_tracks(tracks, false, cx));
        };
        match starred {
            true => apply(cx),
            false => Self::ask(Kind::LibrarySongs(tracks_len), apply, cx),
        }
    }

    pub fn playlist_songs(ids: Vec<String>, detail: Entity<Detail>, count: usize, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        Self::ask(
            Kind::PlaylistSongs(count),
            move |cx| {
                detail.update(cx, |detail, cx| detail.remove_tracks_from_playlist(ids, cx));
            },
            cx,
        );
    }

    pub fn history_songs(tracks: Vec<Track>, history: Entity<History>, cx: &mut App) {
        if tracks.is_empty() {
            return;
        }
        Self::ask(
            Kind::History(tracks.len()),
            move |cx| {
                history.update(cx, |history, cx| {
                    for track in &tracks {
                        history.remove(track, cx);
                    }
                });
            },
            cx,
        );
    }

    pub fn albums(albums: Vec<Album>, cx: &mut App) {
        if albums.is_empty() {
            return;
        }
        let count = albums.len();
        let starred = albums
            .first()
            .is_some_and(|album| Self::unstarring(&album.id, cx));
        let apply = move |cx: &mut App| {
            let library = Sonora::global(cx).library.clone();
            library.update(cx, |library, cx| {
                for album in albums {
                    library.toggle_album(album, cx);
                }
            });
        };
        match starred {
            true => apply(cx),
            false => Self::ask(Kind::Albums(count), apply, cx),
        }
    }

    pub fn artists(artists: Vec<SavedArtist>, cx: &mut App) {
        if artists.is_empty() {
            return;
        }
        let count = artists.len();
        let starred = artists
            .first()
            .is_some_and(|artist| Self::unstarring(&artist.id, cx));
        let apply = move |cx: &mut App| {
            let library = Sonora::global(cx).library.clone();
            library.update(cx, |library, cx| {
                for artist in artists {
                    library.toggle_artist(artist, cx);
                }
            });
        };
        match starred {
            true => apply(cx),
            false => Self::ask(Kind::Artists(count), apply, cx),
        }
    }

    pub fn playlists(ids: Vec<String>, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        Self::ask(
            Kind::Playlists(ids.len()),
            move |cx| {
                let library = Sonora::global(cx).library.clone();
                library.update(cx, |library, cx| {
                    for id in ids {
                        library.remove_playlist_from_library(id, cx);
                    }
                });
            },
            cx,
        );
    }

    /// Remove offline copies of `ids`. Confirms when more than one track is involved.
    pub fn offline_tracks(ids: Vec<String>, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        let offline = state::Offline::global(cx);
        let size = music::offline::format_bytes(offline.read(cx).size_of(&ids));
        let count = ids.len();
        let apply = move |cx: &mut App| {
            state::Offline::global(cx).update(cx, |offline, cx| offline.remove_tracks(ids, cx));
        };
        if count == 1 {
            apply(cx);
            return;
        }
        Self::ask(
            Kind::OfflineTracks {
                count,
                size: size.into(),
            },
            apply,
            cx,
        );
    }

    /// Remove every offline copy of one album.
    pub fn offline_album(
        album_id: Option<String>,
        album_name: String,
        cx: &mut App,
    ) {
        let ids = match album_id.as_deref() {
            Some(id) => music::offline::tracks_for_album(Some(id), &album_name),
            None => music::offline::tracks_for_album(None, &album_name),
        };
        if ids.is_empty() {
            return;
        }
        let size = music::offline::format_bytes(music::offline::size_of(&ids));
        let count = ids.len();
        let name = album_name.clone();
        Self::ask(
            Kind::OfflineAlbum {
                name: name.into(),
                count,
                size: size.into(),
            },
            move |cx| {
                state::Offline::global(cx).update(cx, |offline, cx| offline.remove_tracks(ids, cx));
            },
            cx,
        );
    }

    /// Remove the offline copies of every track on a playlist or album detail page.
    pub fn offline_collection(name: String, ids: Vec<String>, playlist: bool, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        let size = music::offline::format_bytes(music::offline::size_of(&ids));
        let count = ids.len();
        let kind = if playlist {
            Kind::OfflinePlaylist {
                name: name.into(),
                count,
                size: size.into(),
            }
        } else {
            Kind::OfflineAlbum {
                name: name.into(),
                count,
                size: size.into(),
            }
        };
        Self::ask(
            kind,
            move |cx| {
                state::Offline::global(cx).update(cx, |offline, cx| offline.remove_tracks(ids, cx));
            },
            cx,
        );
    }

    /// Wipe every ready offline copy. Stronger wording than a single-album clear.
    pub fn offline_all(cx: &mut App) {
        let offline = state::Offline::global(cx);
        let ids: Vec<String> = offline
            .read(cx)
            .tracks()
            .iter()
            .filter_map(|track| track.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let usage = offline.read(cx).usage();
        let size = music::offline::format_bytes(usage.bytes);
        let count = ids.len();
        Self::ask(
            Kind::OfflineAll {
                count,
                size: size.into(),
            },
            move |cx| {
                state::Offline::global(cx).update(cx, |offline, cx| offline.remove_tracks(ids, cx));
            },
            cx,
        );
    }

        pub fn delete_track_files(ids: Vec<String>, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        Self::ask(
            Kind::DeleteTrackFiles(ids.len()),
            move |cx| {
                let sonora = Sonora::global(cx);
                let Some(provider) = sonora.session.read(cx).local_client() else {
                    return;
                };
                let library = sonora.library.clone();
                let playback = sonora.playback.clone();
                let io = Io::global(cx);
                cx.spawn(async move |cx| {
                    let result = io
                        .spawn(async move {
                            let mut failed = 0;
                            let mut deleted = Vec::new();
                            for id in ids {
                                match provider.delete_track_file(&id).await {
                                    Ok(()) => deleted.push(id),
                                    Err(error) => {
                                        failed += 1;
                                        log::warn!(
                                            "local: cannot delete track file {id}: {error:#}"
                                        );
                                    }
                                }
                            }
                            (failed, deleted)
                        })
                        .await;

                    if let Ok((_, ref deleted)) = result {
                        library.update(cx, |library, cx| library.hide_local_tracks(deleted, cx));
                        playback.update(cx, |playback, cx| playback.remove_from_queue(deleted, cx));
                    }
                    match result {
                        Ok((failed, _)) if failed > 0 => {
                            cx.update(|cx| {
                                Toasts::show(Outcome::Failed, "toast-local-delete-failed", cx);
                            });
                        }
                        Err(error) => {
                            log::warn!("local: deletion task failed: {error}");
                            cx.update(|cx| {
                                Toasts::show(Outcome::Failed, "toast-local-delete-failed", cx);
                            });
                        }
                        Ok(_) => {}
                    }
                })
                .detach();
            },
            cx,
        );
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        self.grab = false;
        if let Some(focus) = self.restore.take() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.grab = false;
        if let Some(focus) = self.restore.take() {
            window.focus(&focus, cx);
        }
        (pending.apply)(cx);
        cx.notify();
    }
}

impl Render for Confirm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        if self.grab {
            self.restore = window.focused(cx);
            window.focus(&self.focus, cx);
            self.grab = false;
        }

        let title = pending.kind.title();
        let detail = pending.kind.detail();
        let action = pending.kind.action();

        div()
            .absolute()
            .inset_0()
            .key_context(FORM_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                cx.stop_propagation();
                this.close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Submit, window, cx| {
                cx.stop_propagation();
                this.apply(window, cx);
            }))
            .child(
                Modal::new("confirm-remove", title)
                    .detail(detail)
                    .action(
                        Button::new("cancel-confirm")
                            .ghost()
                            .label(t!("common-cancel"))
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    )
                    .action(
                        Button::new("apply-confirm")
                            .destructive()
                            .label(action)
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    )
                    .on_dismiss(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
            .into_any_element()
    }
}
