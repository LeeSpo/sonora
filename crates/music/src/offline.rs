//! Offline audio kept on disk for Subsonic / Navidrome playback without the network.

pub use crate::subsonic::offline::{CachedTrack, any_ready, cached_file, cancel, contains, is_cached, list, path, reload, remove, save};
