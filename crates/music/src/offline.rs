//! Offline audio kept on disk for Subsonic / Navidrome playback without the network.

pub use crate::subsonic::offline::{
    CachedLyrics, CachedTrack, any_ready, cached_file, cancel, contains, cover, is_cached, list,
    lyrics, path, reload, remove, save, store_lyrics,
};
