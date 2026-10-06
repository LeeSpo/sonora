//! Offline audio kept on disk for Subsonic / Navidrome playback without the network.

pub use crate::subsonic::offline::{
    Backfill, CachedLyrics, CachedTrack, NAME_ARTIST_PREFIX, any_ready, artist_id_for_name,
    backfill, cached_file, cancel, contains, cover, credited, is_cached, is_name_artist_id, list,
    lyrics, missing_artists, name_artist_id, name_of_artist_id, path, reload, remember, remove,
    same_name, save, split_artists, store_lyrics,
};
