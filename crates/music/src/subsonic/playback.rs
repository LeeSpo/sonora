//! Playback for a Subsonic server: what [`crate::engine`] needs that is Subsonic's own.
//!
//! Almost nothing, as it turns out. The server hands over an ordinary audio file, so the stream
//! takes the bytes as they come and rodio decodes them. The threads, the queue, the preload and
//! the gapless join are the engine's. When the listener has saved a track offline, the decoder
//! opens over the file on disk instead of buffering it in RAM.

use std::fs::File;
use std::io::BufReader;
use std::num::NonZero;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use rodio::Source as _;
use rodio::source::SeekError;

use crate::engine::{self, Fetch, Loudness};
use crate::stream::{Plain, Reader, Stream};
use crate::subsonic::client::{Details, SubsonicClient};
use crate::subsonic::offline;
use crate::{PlaybackConfig, PlaybackEvents, PlaybackFactory, Player};

/// A track ready to decode: either a live stream or a finished offline file.
#[derive(Clone)]
pub struct Loaded {
    kind: Kind,
    details: Details,
}

#[derive(Clone)]
enum Kind {
    Stream(Stream),
    File(PathBuf),
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

/// Decoder over either a network stream or an offline file.
pub enum Out {
    Stream(rodio::Decoder<Reader>),
    File(rodio::Decoder<BufReader<File>>),
}

impl Iterator for Out {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Stream(decoder) => decoder.next(),
            Self::File(decoder) => decoder.next(),
        }
    }
}

impl rodio::Source for Out {
    fn current_span_len(&self) -> Option<usize> {
        match self {
            Self::Stream(decoder) => decoder.current_span_len(),
            Self::File(decoder) => decoder.current_span_len(),
        }
    }

    fn channels(&self) -> NonZero<u16> {
        match self {
            Self::Stream(decoder) => decoder.channels(),
            Self::File(decoder) => decoder.channels(),
        }
    }

    fn sample_rate(&self) -> NonZero<u32> {
        match self {
            Self::Stream(decoder) => decoder.sample_rate(),
            Self::File(decoder) => decoder.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<Duration> {
        match self {
            Self::Stream(decoder) => decoder.total_duration(),
            Self::File(decoder) => decoder.total_duration(),
        }
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        match self {
            Self::Stream(decoder) => decoder.try_seek(pos),
            Self::File(decoder) => decoder.try_seek(pos),
        }
    }
}

fn decode_file(path: &std::path::Path) -> Result<rodio::Decoder<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let meta_len = file.metadata().ok().map(|meta| meta.len());
    let mut builder = rodio::Decoder::builder()
        .with_data(BufReader::new(file))
        .with_seekable(true);
    if let Some(len) = meta_len {
        builder = builder.with_byte_len(len);
    }
    builder
        .build()
        .with_context(|| format!("cannot decode {}", path.display()))
}

#[async_trait]
impl Fetch for Subsonic {
    type Loaded = Loaded;
    type Source = Out;

    fn name(&self) -> &'static str {
        "subsonic"
    }

    /// Opens the stream and asks for the length and loudness at the same time, so neither round
    /// trip waits on the other. The preroll usually covers the lookup entirely. A saved offline
    /// copy skips the network and opens from disk.
    async fn load(&self, id: &str) -> Result<Loaded> {
        if let Some(path) = offline::cached_file(id) {
            let path_for_probe = path.clone();
            let recorded = offline::cached_duration(id);
            let (length, loudness) = tokio::task::spawn_blocking(move || {
                let length = decode_file(&path_for_probe)?.total_duration();
                let loudness = crate::local::tags::loudness(&path_for_probe);
                Ok::<_, anyhow::Error>((length, loudness))
            })
            .await
            .context("cannot probe the offline file")??;

            let details = Details {
                duration: recorded.or(length),
                loudness,
            };
            return Ok(Loaded {
                kind: Kind::File(path),
                details,
            });
        }

        let (stream, details) = tokio::join!(
            async { Stream::open(self.client.open_stream(id).await?, Plain).await },
            self.client.details(id),
        );
        Ok(Loaded {
            kind: Kind::Stream(stream?),
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
        if let Kind::Stream(stream) = &loaded.kind {
            stream.finished().await;
        }
    }

    /// Builds a decoder over a stream or over the offline file and places it at `at`.
    fn open(&self, id: &str, loaded: &Loaded, at: Duration) -> Option<Self::Source> {
        let mut decoder = match &loaded.kind {
            Kind::Stream(stream) => {
                let mut builder = rodio::Decoder::builder()
                    .with_data(stream.reader())
                    .with_seekable(true);
                if let Some(total) = stream.total() {
                    builder = builder.with_byte_len(total);
                }
                match builder.build() {
                    Ok(decoder) => Out::Stream(decoder),
                    Err(error) => {
                        log::warn!("playback: cannot decode the subsonic track {id}: {error}");
                        return None;
                    }
                }
            }
            Kind::File(path) => match decode_file(path) {
                Ok(decoder) => Out::File(decoder),
                Err(error) => {
                    log::warn!("playback: cannot decode offline track {id}: {error:#}");
                    return None;
                }
            },
        };
        if !at.is_zero()
            && let Err(error) = decoder.try_seek(at)
        {
            log::warn!(
                "playback: cannot start the subsonic track {id} at {}s: {error}",
                at.as_secs()
            );
        }
        Some(decoder)
    }
}
