//! Playback for a Subsonic server: what [`crate::engine`] needs that is Subsonic's own.
//!
//! Almost nothing, as it turns out. The server hands over an ordinary audio file, so the stream
//! takes the bytes as they come and rodio decodes them. The threads, the queue, the preload and
//! the gapless join are the engine's. When the listener has saved a track offline, the bytes
//! come from disk instead of the network.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

use crate::engine::{self, Fetch, Loudness};
use crate::stream::{Plain, Reader, Source, Stream};
use crate::subsonic::client::{Details, SubsonicClient};
use crate::subsonic::offline;
use crate::{PlaybackConfig, PlaybackEvents, PlaybackFactory, Player};

/// A track downloading, and what the server says about its length and loudness.
#[derive(Clone)]
pub struct Loaded {
    stream: Stream,
    details: Details,
}

pub struct Factory {
    client: SubsonicClient,
}

impl Factory {
    pub fn new(client: SubsonicClient) -> Self {
        Self { client }
    }
}

impl PlaybackFactory for Factory {
    fn start(&self, config: PlaybackConfig) -> (Box<dyn Player>, Box<dyn PlaybackEvents>) {
        engine::start(
            Subsonic {
                client: self.client.clone(),
            },
            config,
        )
    }
}

struct Subsonic {
    client: SubsonicClient,
}

/// One complete body already on disk, served as a single chunk.
struct CachedBytes(Option<Bytes>);

impl Source for CachedBytes {
    async fn chunk(&mut self) -> Result<Option<Bytes>> {
        Ok(self.0.take())
    }
}

#[async_trait]
impl Fetch for Subsonic {
    type Loaded = Loaded;
    type Source = rodio::Decoder<Reader>;

    fn name(&self) -> &'static str {
        "subsonic"
    }

    /// Opens the stream and asks for the length and loudness at the same time, so neither round
    /// trip waits on the other. The preroll usually covers the lookup entirely. A saved offline
    /// copy skips the network entirely.
    async fn load(&self, id: &str) -> Result<Loaded> {
        if let Some((_path, bytes)) = offline::read_cached(id) {
            let total = bytes.len() as u64;
            let stream = Stream::pulling(CachedBytes(Some(Bytes::from(bytes))), Some(total), Plain)
                .primed()
                .await?;
            // Stay off the network entirely: length comes from what we stored at save time.
            let details = Details {
                duration: offline::cached_duration(id),
                loudness: None,
            };
            return Ok(Loaded { stream, details });
        }

        let (stream, details) = tokio::join!(
            async { Stream::open(self.client.open_stream(id).await?, Plain).await },
            self.client.details(id),
        );
        Ok(Loaded {
            stream: stream?,
            details,
        })
    }

    fn length(&self, loaded: &Loaded) -> Option<Duration> {
        loaded.details.duration
    }

    fn loudness(&self, loaded: &Loaded) -> Option<Loudness> {
        loaded.details.loudness
    }

    async fn downloaded(&self, loaded: &Loaded) {
        loaded.stream.finished().await;
    }

    /// Builds a decoder over a stream and places it at `at`. The bytes past the preroll are
    /// still arriving, so this only reads the header.
    fn open(&self, id: &str, loaded: &Loaded, at: Duration) -> Option<Self::Source> {
        let mut builder = rodio::Decoder::builder()
            .with_data(loaded.stream.reader())
            .with_seekable(true);
        if let Some(total) = loaded.stream.total() {
            builder = builder.with_byte_len(total);
        }
        let mut decoder = match builder.build() {
            Ok(decoder) => decoder,
            Err(error) => {
                log::warn!("playback: cannot decode the subsonic track {id}: {error}");
                return None;
            }
        };
        if !at.is_zero()
            && let Err(error) = rodio::Source::try_seek(&mut decoder, at)
        {
            log::warn!(
                "playback: cannot start the subsonic track {id} at {}s: {error}",
                at.as_secs()
            );
        }
        Some(decoder)
    }
}
