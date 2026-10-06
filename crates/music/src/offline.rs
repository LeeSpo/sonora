//! Offline audio kept on disk for Subsonic / Navidrome playback without the network.

pub use crate::subsonic::offline::{
    AlbumGroup, Backfill, CachedLyrics, CachedTrack, NAME_ARTIST_PREFIX, Usage, albums, any_ready,
    artist_id_for_name, backfill, cached_file, cancel, contains, cover, credited, format_bytes,
    is_cached, is_name_artist_id, list, lyrics, missing_artists, name_artist_id, name_of_artist_id,
    path, reload, remember, remove, same_name, save, size_of, split_artists, store_lyrics,
    tracks_for_album, usage,
};
