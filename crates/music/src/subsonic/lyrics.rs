//! Lyrics from a Subsonic / Navidrome server via `getLyricsBySongId`.
//!
//! Feishin treats these as the track's own (embedded / `.lrc` / server-scanned) sheets.
//! Sonora previously only asked internet providers, so Navidrome lyrics never appeared.

use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use opensubsonic::data::{CueLine, StructuredLyrics};

use crate::lyrics::lrc;
use crate::subsonic::auth;
use crate::subsonic::client::SubsonicClient;
use crate::{Lyrics, LyricsHit, LyricsLine, LyricsProvider, LyricsQuery, LyricsWord, Voice};

const SOURCE: &str = "Subsonic";
const SLUG: &str = "subsonic";
/// Ahead of generic internet matches, behind a truly worded karaoke sheet from elsewhere.
const TRUST: u32 = 120;

pub struct SubsonicLyrics;

impl SubsonicLyrics {
    pub fn new() -> Self {
        Self
    }

    fn client() -> Result<Option<SubsonicClient>> {
        let Some(mut remembered) = auth::load() else {
            return Ok(None);
        };
        if remembered.signature.is_none() {
            remembered.signature = Some(auth::sign(&remembered.username, &remembered.password));
        }
        let signature = remembered.signature.clone().unwrap_or_default();
        Ok(Some(SubsonicClient::new(
            remembered.server,
            remembered.username,
            remembered.password,
            &signature,
        )?))
    }
}

impl Default for SubsonicLyrics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LyricsProvider for SubsonicLyrics {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Vec<LyricsHit>> {
        let Some(id) = query.id_for(SLUG) else {
            return Ok(Vec::new());
        };
        let Some(client) = Self::client()? else {
            return Ok(Vec::new());
        };

        let list = match client.lyrics(id, true).await {
            Ok(list) => list,
            Err(error) => {
                log::debug!("subsonic lyrics: enhanced lookup failed ({error:#}); retrying plain");
                client
                    .lyrics(id, false)
                    .await
                    .with_context(|| format!("cannot load lyrics for {id}"))?
            }
        };

        Ok(list
            .structured_lyrics
            .into_iter()
            .filter_map(|sheet| hit(query, sheet))
            .collect())
    }
}

fn hit(query: &LyricsQuery, sheet: StructuredLyrics) -> Option<LyricsHit> {
    let kind = sheet.kind.as_deref().unwrap_or("main");
    // Translations / pronunciations are kept but ranked lower via trust.
    let trust = match kind {
        "main" | "" => TRUST,
        _ => TRUST / 2,
    };

    let lyrics = convert(sheet)?;
    if lyrics.is_empty() {
        return None;
    }

    Some(LyricsHit {
        source: SOURCE,
        trust,
        lyrics,
        instrumental: false,
        title: query.title.clone(),
        artist: query.artist.clone(),
        album: query.album.clone(),
        duration: Some(query.duration),
        writers: Vec::new(),
    })
}

fn convert(sheet: StructuredLyrics) -> Option<Lyrics> {
    let offset = sheet.offset.unwrap_or(0.0);
    if sheet.synced {
        let cues = sheet.cue_line.as_deref().unwrap_or(&[]);
        let mut lines: Vec<LyricsLine> = sheet
            .line
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let start_ms = (line.start.unwrap_or(0.0) + offset).max(0.0) as u64;
                LyricsLine {
                    start: Duration::from_millis(start_ms),
                    end: None,
                    text: line.value,
                    romanized: None,
                    words: words_for(cues, index, offset),
                    secondary: Vec::new(),
                    voice: Voice::Lead,
                }
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        lrc::normalize(&mut lines);
        Some(Lyrics::Synced {
            lines: lines.into(),
        })
    } else {
        let text = sheet
            .line
            .into_iter()
            .map(|line| line.value)
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Lyrics::plain(trimmed.to_owned()))
    }
}

fn words_for(cues: &[CueLine], index: usize, offset: f64) -> Option<Vec<LyricsWord>> {
    let matching: Vec<&CueLine> = cues
        .iter()
        .filter(|cue| cue.index.map(|at| at as usize) == Some(index))
        .collect();
    let cue = matching.first().copied().or_else(|| {
        // Some servers omit index and return one cue line per lyric line.
        (cues.len() > index && cues[index].index.is_none()).then(|| &cues[index])
    })?;
    let words: Vec<LyricsWord> = cue
        .cue
        .iter()
        .filter_map(|word| {
            let text = word.value.clone().filter(|text| !text.is_empty())?;
            let start_ms = (word.start.unwrap_or(0.0) + offset).max(0.0) as u64;
            let end_ms = word
                .end
                .map(|end| (end + offset).max(0.0) as u64)
                .unwrap_or(start_ms);
            Some(LyricsWord {
                start: Duration::from_millis(start_ms),
                end: Duration::from_millis(end_ms),
                text,
            })
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

#[cfg(test)]
mod tests {
    use opensubsonic::data::Line;

    use super::*;

    #[test]
    fn converts_synced_lines() {
        let sheet = StructuredLyrics {
            lang: "en".into(),
            synced: true,
            line: vec![
                Line {
                    value: "One".into(),
                    start: Some(1000.0),
                },
                Line {
                    value: "Two".into(),
                    start: Some(2000.0),
                },
            ],
            display_artist: None,
            display_title: None,
            offset: Some(-200.0),
            kind: Some("main".into()),
            agents: None,
            cue_line: None,
        };
        let Lyrics::Synced { lines } = convert(sheet).unwrap() else {
            panic!("expected synced");
        };
        assert_eq!(lines[0].text, "One");
        assert_eq!(lines[0].start, Duration::from_millis(800));
        assert_eq!(lines[1].start, Duration::from_millis(1800));
    }

    #[test]
    fn converts_plain_lines() {
        let sheet = StructuredLyrics {
            lang: "und".into(),
            synced: false,
            line: vec![
                Line {
                    value: "Hello".into(),
                    start: None,
                },
                Line {
                    value: "World".into(),
                    start: None,
                },
            ],
            display_artist: None,
            display_title: None,
            offset: None,
            kind: None,
            agents: None,
            cue_line: None,
        };
        let Lyrics::Plain { text, .. } = convert(sheet).unwrap() else {
            panic!("expected plain");
        };
        assert_eq!(text, "Hello\nWorld");
    }
}
