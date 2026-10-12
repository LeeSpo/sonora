//! Playback for a Subsonic server: what [`crate::engine`] needs that is Subsonic's own.
//!
//! Almost nothing, as it turns out. The server hands over an ordinary audio file, so the stream
//! takes the bytes as they come and rodio decodes them. The threads, the queue, the preload and
//! the gapless join are the engine's. When the listener has saved a track offline, the decoder
//! opens over the file on disk instead of buffering it in RAM.

use std::io::BufReader;
use std::num::NonZero;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use rodio::Source as _;
use rodio::source::SeekError;

use crate::engine::{self, Fetch, Loudness};
use crate::local::playback::{self, Audio};
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
    File(rodio::Decoder<BufReader<Audio>>),
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

/// Opens a saved copy for decoding. It is the server's original file, with whatever tags and
/// headers the uploader left in it, so it takes the same path as a file from the library.
fn decode_file(path: &std::path::Path) -> Result<rodio::Decoder<BufReader<Audio>>> {
    playback::decode_path(path).with_context(|| format!("cannot decode {}", path.display()))
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
                duration: recorded.filter(|recorded| !recorded.is_zero()).or(length),
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

#[cfg(test)]
mod tests {
    //! A saved copy is the server's original file, so these are real-shaped files: a short tone
    //! in each format, then dressed up the way libraries dress them (a big embedded cover, an
    //! ID3v2 tag in front, an MP3 header that miscounts its own frames). Each one has to decode
    //! all the way to its end, not just for the first few seconds.

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt as _;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::probe::Probe;
    use lofty::tag::{Tag, TagExt as _};

    use super::*;

    const MP3: &[u8] = include_bytes!("testdata/tone.mp3");
    const FLAC: &[u8] = include_bytes!("testdata/tone.flac");
    const M4A: &[u8] = include_bytes!("testdata/tone.m4a");
    /// Every tone is this long.
    const LENGTH: Duration = Duration::from_secs(20);

    fn scratch(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.audio"));
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    /// An ID3v2.3 tag of `size` bytes whose body looks like a cover picture: noise with MPEG
    /// frame sync patterns in it, which is what makes a decoder mistake it for audio.
    fn id3v2(size: usize) -> Vec<u8> {
        let mut body: Vec<u8> = (0..size)
            .map(|n| match n % 7 {
                0 => 0xff,
                1 => 0xfb,
                2 => 0x90,
                3 => 0x44,
                n => (n * 31) as u8,
            })
            .collect();
        body[..11].copy_from_slice(b"\0image/jpeg");
        let mut tag = b"ID3\x03\x00\x00".to_vec();
        tag.extend([
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        tag.extend(body);
        tag
    }

    /// The MP3 with the frame count in its Xing/Info header replaced.
    fn with_frame_count(frames: u32) -> Vec<u8> {
        let mut mp3 = MP3.to_vec();
        let at = mp3
            .windows(4)
            .position(|window| window == b"Info" || window == b"Xing")
            .expect("the fixture has a Xing/Info header");
        assert_eq!(mp3[at + 7] & 1, 1, "the header carries a frame count");
        mp3[at + 8..at + 12].copy_from_slice(&frames.to_be_bytes());
        mp3
    }

    /// Embeds a `bytes`-long picture in the file's own tag.
    fn with_picture(path: &Path, bytes: usize) {
        let mut tagged = Probe::open(path)
            .unwrap()
            .guess_file_type()
            .unwrap()
            .read()
            .unwrap();
        let tag_type = tagged.primary_tag_type();
        if tagged.tag(tag_type).is_none() {
            tagged.insert_tag(Tag::new(tag_type));
        }
        let tag = tagged.tag_mut(tag_type).unwrap();
        let noise = (0..bytes).map(|n| (n * 31 % 251) as u8).collect();
        tag.push_picture(
            Picture::unchecked(noise)
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    /// How much audio the decoder yields before it ends.
    fn decoded(path: &Path) -> Duration {
        let decoder = decode_file(path).unwrap();
        let channels = f64::from(decoder.channels().get());
        let rate = f64::from(decoder.sample_rate().get());
        let samples = decoder.count() as f64;
        Duration::from_secs_f64(samples / channels / rate)
    }

    fn assert_whole(path: &Path, what: &str) {
        let heard = decoded(path);
        assert!(
            heard.abs_diff(LENGTH) < Duration::from_secs(1),
            "{what}: decoded {heard:?} of {LENGTH:?}"
        );
    }

    #[test]
    fn a_plain_file_plays_through() {
        for (name, bytes) in [("mp3", MP3), ("flac", FLAC), ("m4a", M4A)] {
            let (_dir, path) = scratch(name, bytes);
            assert_whole(&path, name);
        }
    }

    #[test]
    fn an_mp3_behind_a_large_id3v2_tag_plays_through() {
        let mut bytes = id3v2(600_000);
        bytes.extend_from_slice(MP3);
        let (_dir, path) = scratch("tagged", &bytes);
        assert_whole(&path, "mp3 behind a 600 kB ID3v2 tag");
    }

    #[test]
    fn an_mp3_whose_header_counts_too_few_frames_plays_through() {
        // 100 frames is about 7 s of this tone
        let (_dir, path) = scratch("short", &with_frame_count(100));
        assert_whole(&path, "mp3 with a Xing count of 100 frames");

        let mut bytes = id3v2(500_000);
        bytes.extend(with_frame_count(100));
        let (_dir, path) = scratch("short-tagged", &bytes);
        assert_whole(&path, "tagged mp3 with a Xing count of 100 frames");
    }

    #[test]
    fn an_mp3_whose_header_counts_no_frames_plays_through() {
        let (_dir, path) = scratch("zero", &with_frame_count(0));
        assert_whole(&path, "mp3 with a Xing count of 0 frames");
    }

    #[test]
    fn an_mp3_whose_header_counts_right_keeps_its_length() {
        let (_dir, path) = scratch("honest", MP3);
        assert!(
            !crate::local::wire::has_lying_xing_frame_count(&path, 0),
            "the fixture's own count is right"
        );
    }

    #[test]
    fn flac_and_m4a_with_a_large_cover_play_through() {
        for (name, bytes) in [("flac", FLAC), ("m4a", M4A)] {
            let (_dir, path) = scratch(name, bytes);
            with_picture(&path, 500_000);
            assert!(std::fs::metadata(&path).unwrap().len() > 500_000);
            assert_whole(&path, &format!("{name} with a 500 kB cover"));
        }
    }

    #[test]
    fn a_flac_behind_an_id3v2_tag_plays_through() {
        let mut bytes = id3v2(300_000);
        bytes.extend_from_slice(FLAC);
        let (_dir, path) = scratch("tagged-flac", &bytes);
        assert_whole(&path, "flac behind an ID3v2 tag");
    }

    #[test]
    fn seeking_lands_inside_the_audio_not_the_tag() {
        let mut bytes = id3v2(400_000);
        bytes.extend_from_slice(MP3);
        let (_dir, path) = scratch("seek", &bytes);
        let mut decoder = decode_file(&path).unwrap();
        decoder.try_seek(Duration::from_secs(12)).unwrap();
        let channels = f64::from(decoder.channels().get());
        let rate = f64::from(decoder.sample_rate().get());
        let left = Duration::from_secs_f64(decoder.count() as f64 / channels / rate);
        assert!(
            left.abs_diff(Duration::from_secs(8)) < Duration::from_secs(1),
            "{left:?} left after seeking to 12 s of {LENGTH:?}"
        );
    }
}
