//! Offline audio kept on disk for Subsonic / Navidrome playback without the network.

pub use crate::subsonic::offline::{CachedTrack, any_ready, contains, is_cached, list, path, remove, save};
