//! Play saved Subsonic / Navidrome tracks when the network is gone, without opening a
//! detail page that cannot load.

use gpui::{App, Context, Entity};
use music::Track;
use state::{Network, Offline, Origin, Playback};

/// Whether `album` has at least one track ready on disk.
pub(crate) fn album_cached(album_id: &str, album_name: &str) -> bool {
    !music::offline::tracks_for_album(Some(album_id), album_name).is_empty()
}

/// Whether `artist` has saved tracks that the offline artist page can show.
pub(crate) fn artist_cached(artist_id: &str, cx: &App) -> bool {
    Offline::global(cx).read(cx).has_artist(artist_id)
}

/// Queue an album's ready offline tracks and start playback. Returns whether anything played.
pub(crate) fn play_album(
    album_id: &str,
    album_name: &str,
    playback: &Entity<Playback>,
    cx: &mut App,
) -> bool {
    playback.update(cx, |playback, cx| start_album(playback, album_id, album_name, cx))
}

/// Start an album from disk inside a playback update. Returns whether anything played.
pub(crate) fn start_album(
    playback: &mut Playback,
    album_id: &str,
    album_name: &str,
    cx: &mut Context<Playback>,
) -> bool {
    let tracks = cached_album_tracks(album_id, album_name, cx);
    if tracks.is_empty() {
        return false;
    }
    let origin = Origin::album(album_id.to_owned()).named(album_name.to_owned());
    playback.start(tracks, 0, Some(origin), cx);
    true
}

/// Toggle or start an album from its offline tracks when the network is gone.
pub(crate) fn toggle_album(
    album_id: &str,
    album_name: &str,
    playback: &Entity<Playback>,
    cx: &mut App,
) -> bool {
    if !Network::lost(cx) {
        return false;
    }
    let origin = Origin::album(album_id.to_owned()).named(album_name.to_owned());
    if playback.read(cx).playing_from(&origin).is_some() {
        playback.update(cx, |playback, cx| playback.toggle_play(cx));
        return true;
    }
    play_album(album_id, album_name, playback, cx)
}

fn cached_album_tracks(album_id: &str, album_name: &str, cx: &App) -> Vec<Track> {
    let want: std::collections::HashSet<String> =
        music::offline::tracks_for_album(Some(album_id), album_name)
            .into_iter()
            .collect();
    if want.is_empty() {
        // Fall back to name match when the album id was never stored on older saves.
        let offline = Offline::global(cx).read(cx);
        let mut tracks: Vec<Track> = offline
            .tracks()
            .iter()
            .filter(|track| {
                !album_name.is_empty() && music::offline::same_name(&track.album, album_name)
            })
            .cloned()
            .collect();
        sort_album(&mut tracks);
        return tracks;
    }
    let offline = Offline::global(cx).read(cx);
    let mut tracks: Vec<Track> = offline
        .tracks()
        .iter()
        .filter(|track| track.id.as_ref().is_some_and(|id| want.contains(id)))
        .cloned()
        .collect();
    sort_album(&mut tracks);
    tracks
}

fn sort_album(tracks: &mut [Track]) {
    tracks.sort_by_key(|track| {
        (
            track.disc_number,
            track.track_number,
            track.name.to_lowercase(),
        )
    });
}
