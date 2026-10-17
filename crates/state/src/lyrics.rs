use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, Entity, Task};
use music::lyrics::LOCAL;
use music::{Lyrics as Sheet, LyricsHit, LyricsProvider, LyricsQuery, Track, TrackKey};
use tokio::task::JoinSet;

use crate::sheets::{Sheets, Unsaved};
use crate::{AppSettings, Io, Playback, Queue, Session, join};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LyricsState {
    Idle,
    Loading,
    Ready,
    Instrumental,
    Missing,
    Failed(String),
}

const SAVE_DELAY: Duration = Duration::from_millis(800);

/// How many tracks keep their answer in memory. The rest are read back from the cache database,
/// so this only has to cover the track playing, the next one and a few just played.
const RECENT: usize = 16;

pub struct Lyrics {
    state: LyricsState,
    hits: Vec<LyricsHit>,
    chosen: usize,
    picked: bool,
    settled: bool,
    revision: u64,
    following: Option<String>,
    /// The answers looked at most recently, newest first.
    recent: VecDeque<(String, Found)>,
    store: Sheets,
    /// Answers found since the last save, written together once lookups go quiet.
    unsaved: Vec<Unsaved>,
    providers: Vec<Arc<dyn LyricsProvider>>,
    enabled_providers: Vec<String>,
    prefer_local: bool,
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    session: Entity<Session>,
    settings: Entity<AppSettings>,
    io: Io,
    task: Option<Task<()>>,
    ahead: Option<Task<()>>,
    ahead_of: Option<String>,
    save: Option<Task<()>>,
    /// Moves the old lyrics.json into the cache database, once.
    _migration: Task<()>,
}

impl Lyrics {
    pub fn new(
        playback: Entity<Playback>,
        queue: Entity<Queue>,
        session: Entity<Session>,
        settings: Entity<AppSettings>,
        providers: Vec<Arc<dyn LyricsProvider>>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&playback, |this, _, cx| this.follow(cx))
            .detach();
        cx.observe(&queue, |this, _, cx| this.prefetch(cx)).detach();
        cx.observe(&settings, |this, settings, cx| {
            let settings = settings.read(cx);
            let enabled = settings.lyrics_providers();
            let prefer_local = settings.prefer_local_lyrics();
            if enabled == this.enabled_providers && prefer_local == this.prefer_local {
                return;
            }
            this.enabled_providers = enabled.to_vec();
            this.prefer_local = prefer_local;
            this.task = None;
            this.ahead = None;
            this.ahead_of = None;
            this.forget(cx);
            this.follow(cx);
        })
        .detach();
        let (enabled_providers, prefer_local) = {
            let settings = settings.read(cx);
            (
                settings.lyrics_providers().to_vec(),
                settings.prefer_local_lyrics(),
            )
        };
        let store = Sheets::new(storage::Cache::standard());
        let moving = store.clone();
        let migration = cx
            .background_executor()
            .spawn(async move { moving.migrate() });
        Self {
            state: LyricsState::Idle,
            hits: Vec::new(),
            chosen: 0,
            picked: false,
            settled: false,
            revision: 0,
            following: None,
            recent: VecDeque::new(),
            store,
            unsaved: Vec::new(),
            providers,
            enabled_providers,
            prefer_local,
            playback,
            queue,
            session,
            settings,
            io,
            task: None,
            ahead: None,
            ahead_of: None,
            save: None,
            _migration: migration,
        }
    }

    pub fn state(&self) -> &LyricsState {
        &self.state
    }

    pub fn following(&self) -> Option<&str> {
        self.following.as_deref()
    }

    pub fn hits(&self) -> &[LyricsHit] {
        &self.hits
    }

    pub fn current(&self) -> Option<&LyricsHit> {
        self.hits.get(self.chosen)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.hits.len() || index == self.chosen {
            return;
        }
        self.chosen = index;
        self.picked = true;
        self.revision = self.revision.wrapping_add(1);
        cx.notify();
    }

    pub fn active_line(&self, cx: &Context<Self>) -> Option<usize> {
        let music::Lyrics::Synced { lines } = &self.current()?.lyrics else {
            return None;
        };
        music::lyrics::active(lines, self.playback.read(cx).position())
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.playback.read(cx).track() else {
            return self.forget(cx);
        };
        let Some(id) = track.id.as_deref() else {
            return self.forget(cx);
        };
        if self.following.as_deref() == Some(id) {
            return;
        }
        let (track, id) = (track.clone(), id.to_owned());
        self.following = Some(id.clone());
        self.chosen = 0;
        self.picked = false;
        self.settled = false;
        self.revision = self.revision.wrapping_add(1);

        // A file's own lyrics are never cached, so an edit to its tags shows on the next play.
        if !self.reads_file(&id, cx)
            && let Some(found) = self.recall(&id)
            && self
                .asked(&id, cx)
                .iter()
                .all(|provider| found.covers(provider))
        {
            let query = self.query(&track, &id, cx);
            self.show(&found, &query, cx);
            return;
        }
        self.load(id, track, cx);
    }

    /// Puts what `found` holds from the enabled sources on screen as the settled answer.
    fn show(&mut self, found: &Found, query: &LyricsQuery, cx: &mut Context<Self>) {
        let settings = self.settings.read(cx);
        let hits: Vec<LyricsHit> = found
            .hits
            .iter()
            .filter(|hit| settings.lyrics_provider_enabled(hit.source))
            .cloned()
            .collect();
        let instrumental = music::lyrics::instrumental(query, &hits);
        self.task = None;
        self.settled = true;
        self.hits = ordered(query, hits);
        self.state = state_for(&self.hits, instrumental);
        cx.notify();
        self.prefetch(cx);
    }

    /// The answer held in memory for `id`, which becomes the most recently used.
    fn recall(&mut self, id: &str) -> Option<Found> {
        let index = self.recent.iter().position(|(held, _)| held == id)?;
        let entry = self.recent.remove(index)?;
        let found = entry.1.clone();
        self.recent.push_front(entry);
        Some(found)
    }

    fn recalls(&self, id: &str) -> bool {
        self.recent.iter().any(|(held, _)| held == id)
    }

    /// Holds `found` in memory as the newest answer, letting the oldest go past `RECENT`.
    fn hold(&mut self, id: String, found: Found) {
        self.recent.retain(|(held, _)| *held != id);
        self.recent.push_front((id, found));
        self.recent.truncate(RECENT);
    }

    /// Reads the answer stored for `id` off the main thread.
    fn stored(&self, id: &str, cx: &Context<Self>) -> Task<Option<Found>> {
        let key = self.key(id, cx);
        let known: Vec<&'static str> = self
            .providers
            .iter()
            .map(|provider| provider.name())
            .collect();
        let store = self.store.clone();
        cx.background_executor().spawn(async move {
            store
                .get(&key, &known)
                .map(|(hits, answered)| Found { hits, answered })
        })
    }

    /// One key per track. The stored answer covers every service that has replied, whichever
    /// are enabled now, and the selection is applied when it is shown.
    fn key(&self, id: &str, cx: &Context<Self>) -> String {
        match self.session.read(cx).slug_for(id) {
            Some(slug) => format!("{slug}:{id}:lyrics"),
            None => format!("{id}:lyrics"),
        }
    }

    /// The sheet saved with an offline copy of `id`, as a hit the panel can show.
    fn kept_offline(&self, id: &str) -> Option<LyricsHit> {
        if music::is_local_id(id) {
            return None;
        }
        let kept = music::offline::lyrics(id)?;
        let known: Vec<&'static str> = self
            .providers
            .iter()
            .map(|provider| provider.name())
            .collect();
        Some(offline_hit(kept, &known))
    }

    /// Puts up the sheet saved with an offline copy. It is shown whatever the provider settings
    /// say, since it is all there is without the network, and it is never cached under the
    /// providers' key.
    fn show_offline(&mut self, hit: LyricsHit, cx: &mut Context<Self>) {
        log::debug!("lyrics: showing the sheet saved offline");
        self.task = None;
        self.settled = true;
        self.chosen = 0;
        self.picked = false;
        self.hits = vec![hit];
        self.revision = self.revision.wrapping_add(1);
        self.state = LyricsState::Ready;
        cx.notify();
    }

    /// Saves the winning sheet beside a track that has an offline copy, so it shows without the
    /// network later.
    fn keep_offline(&self, id: &str, ranked: &[LyricsHit], cx: &mut Context<Self>) {
        if music::is_local_id(id) || !music::offline::is_cached(id) {
            return;
        }
        let Some(kept) = sheet_to_keep(ranked) else {
            return;
        };
        let track_id = id.to_owned();
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = music::offline::store_lyrics(&track_id, &kept) {
                    log::debug!("lyrics: cannot keep the sheet offline: {error:#}");
                }
            })
            .detach();
    }

    /// The providers a lookup for `id` may ask. The file's own lyrics are read only for a local
    /// file, and the services only when its metadata may go online.
    fn asked(&self, id: &str, cx: &Context<Self>) -> Vec<Arc<dyn LyricsProvider>> {
        let online = self.online(id, cx);
        let reads_file = self.reads_file(id, cx);
        let settings = self.settings.read(cx);
        self.providers
            .iter()
            .filter(|provider| settings.lyrics_provider_enabled(provider.name()))
            .filter(|provider| match provider.name() == LOCAL {
                true => reads_file,
                false => online,
            })
            .cloned()
            .collect()
    }

    fn query(&self, track: &Track, id: &str, cx: &Context<Self>) -> LyricsQuery {
        let key = self.session.read(cx).slug_for(id).map(|provider| TrackKey {
            provider,
            id: id.to_owned(),
        });
        query_for(track, key)
    }

    /// Whether the services may be asked about this track, which a local file only allows when
    /// the user lets its metadata go online.
    fn online(&self, id: &str, cx: &Context<Self>) -> bool {
        !music::is_local_id(id) || self.settings.read(cx).lyrics_for_local_files()
    }

    fn reads_file(&self, id: &str, cx: &Context<Self>) -> bool {
        music::is_local_id(id) && self.settings.read(cx).lyrics_provider_enabled(LOCAL)
    }

    /// Ends a lookup's bookkeeping; true when it was for the track on screen.
    fn finished(&mut self, id: &str) -> bool {
        let current = self.following.as_deref() == Some(id);
        if current {
            self.task = None;
        }
        if self.ahead_of.as_deref() == Some(id) {
            self.ahead_of = None;
        }
        current
    }

    fn forget(&mut self, cx: &mut Context<Self>) {
        if self.following.is_none() {
            return;
        }
        self.task = None;
        self.following = None;
        self.hits.clear();
        self.chosen = 0;
        self.picked = false;
        self.settled = false;
        self.revision = self.revision.wrapping_add(1);
        self.state = LyricsState::Idle;
        cx.notify();
    }

    fn load(&mut self, id: String, track: Track, cx: &mut Context<Self>) {
        self.hits.clear();
        self.state = LyricsState::Loading;
        cx.notify();

        // No network: a saved track brings its own lyrics, so skip the doomed lookup.
        if crate::Network::lost(cx)
            && let Some(hit) = self.kept_offline(&id)
        {
            self.show_offline(hit, cx);
            return;
        }

        if self.ahead_of.as_deref() == Some(id.as_str()) && self.ahead.is_some() {
            self.task = self.ahead.take();
            self.ahead_of = None;
            return;
        }
        self.task = Some(self.fetch(id, track, cx));
    }

    fn prefetch(&mut self, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        let Some(track) = self.queue.read(cx).upcoming().next() else {
            return;
        };
        let Some(id) = track.id.as_deref() else {
            return;
        };
        if self.ahead_of.as_deref() == Some(id) || self.recalls(id) {
            return;
        }
        let (track, id) = (track.clone(), id.to_owned());
        self.ahead_of = Some(id.clone());
        self.ahead = Some(self.fetch(id, track, cx));
    }

    /// Looks a track up: in memory, then in the cache database, then with the providers. Only
    /// the services that have not answered for this track yet are asked, and a file's own lyrics
    /// are read again on every play. What the cache holds is shown alongside the new answers.
    fn fetch(&mut self, id: String, track: Track, cx: &mut Context<Self>) -> Task<()> {
        let settings = self.settings.read(cx);
        let prefer_local = settings.prefer_local_lyrics();
        let enabled: Vec<&'static str> = self
            .providers
            .iter()
            .map(|provider| provider.name())
            .filter(|name| settings.lyrics_provider_enabled(name))
            .collect();
        let asked = self.asked(&id, cx);
        let remembered = self.recall(&id);
        let stored = remembered.is_none().then(|| self.stored(&id, cx));
        let query = self.query(&track, &id, cx);
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let cached = match stored {
                Some(stored) => stored.await,
                None => remembered,
            };
            if let Some(found) = &cached {
                let held = found.clone();
                let id = id.clone();
                this.update(cx, |this, _| this.hold(id, held)).ok();
            }
            let cached = cached.unwrap_or_default();
            let mut providers: Vec<Arc<dyn LyricsProvider>> = asked
                .into_iter()
                .filter(|provider| !cached.covers(provider))
                .collect();
            if providers.is_empty() {
                this.update(cx, |this, cx| {
                    if this.finished(&id) {
                        log::debug!("lyrics: showing the stored answer");
                        this.show(&cached, &query, cx);
                    }
                })
                .ok();
                return;
            }

            if prefer_local
                && let Some(index) = providers
                    .iter()
                    .position(|provider| provider.name() == LOCAL)
            {
                let local = providers.remove(index);
                let asked = query.clone();
                let read = io.spawn(async move { local.search(&asked).await });
                let hits = join(read).await.unwrap_or_else(|error| {
                    log::warn!(
                        "lyrics: cannot read the lyrics in {}: {error:#}",
                        track.name
                    );
                    Vec::new()
                });
                if hits.iter().any(|hit| music::lyrics::eligible(&query, hit)) {
                    let found = Found {
                        hits,
                        answered: Vec::new(),
                    };
                    this.update(cx, |this, cx| {
                        if this.finished(&id) {
                            this.show(&found, &query, cx);
                        }
                    })
                    .ok();
                    return;
                }
            }

            // The cached hits from enabled sources go in first, as if they had just arrived.
            let held: Vec<LyricsHit> = cached
                .hits
                .iter()
                .filter(|hit| enabled.contains(&hit.source))
                .cloned()
                .collect();
            let (sender, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            if !held.is_empty() {
                sender.send(held).ok();
            }
            let ranking = query.clone();
            let worker = io.spawn(async move { gather(providers, query, sender).await });
            let mut hits = Vec::new();
            let mut displayed: Option<LyricsHit> = None;
            let mut shown: Option<u8> = None;

            while let Some(mut found) = incoming.recv().await {
                hits.append(&mut found);
                let ranked = ordered(&ranking, hits.clone());
                let Some(best) = ranked.first().cloned() else {
                    continue;
                };
                let step = depth(&best.lyrics);
                if shown.is_none_or(|shown| step >= shown) {
                    shown = Some(step);
                    displayed = Some(best);
                }
                let anchor = displayed.clone();
                this.update(cx, |this, cx| {
                    if this.following.as_deref() != Some(id.as_str()) {
                        return;
                    }
                    this.paint(ranked, anchor.as_ref(), cx);
                })
                .ok();
            }

            let answered = join(worker).await;

            this.update(cx, |this, cx| {
                let current = this.finished(&id);
                match answered {
                    Ok(answered) => {
                        let instrumental = music::lyrics::instrumental(&ranking, &hits);
                        // Nothing answered (typically offline): fall back to the saved sheet
                        // without caching anything under the providers' key.
                        if answered.is_empty()
                            && hits.is_empty()
                            && !instrumental
                            && let Some(hit) = this.kept_offline(&id)
                        {
                            if current {
                                this.show_offline(hit, cx);
                            }
                            return;
                        }
                        // Lyrics come from third-party services, so their answering says nothing
                        // about the music provider. Marking the network back here flipped every
                        // screen from the offline notice to a reload (and back) on each lookup.
                        let ranked = ordered(&ranking, hits.clone());
                        this.keep_offline(&id, &ranked, cx);
                        this.keep(id, cached, &hits, answered, cx);
                        if current {
                            this.settle(ranked, displayed.as_ref(), instrumental, cx);
                        }
                    }
                    Err(error) => {
                        log::warn!("lyrics: cannot look up {}: {error:#}", track.name);
                        if current && let Some(hit) = this.kept_offline(&id) {
                            this.show_offline(hit, cx);
                            return;
                        }
                        let reason = crate::blamed(&error, cx);
                        if current {
                            this.state = LyricsState::Failed(reason);
                            cx.notify();
                        }
                    }
                }
            })
            .ok();
        })
    }

    /// Shows an answer that arrived while others are still being looked for.
    ///
    /// A word-by-word sheet is the best there is, so it goes up the moment it
    /// turns up and settles the question rather than waiting for the slowest
    /// source to answer. Short of that, the first timed answer is put up and
    /// nothing replaces it until the search is over: the reader is never walked
    /// through a series of ever better sheets, and an untimed one is not shown at
    /// all while something better could still arrive.
    fn paint(
        &mut self,
        ranked: Vec<LyricsHit>,
        displayed: Option<&LyricsHit>,
        cx: &mut Context<Self>,
    ) {
        if self.settled {
            return;
        }
        let kind = self
            .prospect(&ranked, displayed)
            .map(|hit| depth(&hit.lyrics));
        let best = kind == Some(WORDED);
        if !best && !self.hits.is_empty() {
            return;
        }
        if !best && kind != Some(TIMED) {
            return;
        }
        match best {
            true => {
                log::debug!("lyrics: settling on a word-by-word answer as it arrives");
                self.settled = true;
            }
            false => log::debug!("lyrics: showing the first timed answer while the rest arrive"),
        }
        self.apply(ranked, displayed, cx);
    }

    fn apply(
        &mut self,
        ranked: Vec<LyricsHit>,
        displayed: Option<&LyricsHit>,
        cx: &mut Context<Self>,
    ) {
        self.pin(ranked, displayed);
        if !self.hits.is_empty() {
            self.state = LyricsState::Ready;
        }
        cx.notify();
    }

    /// The hit a ranking would put on screen.
    fn prospect<'a>(
        &'a self,
        ranked: &'a [LyricsHit],
        displayed: Option<&'a LyricsHit>,
    ) -> Option<&'a LyricsHit> {
        let anchor = match self.picked {
            true => self.hits.get(self.chosen),
            false => displayed,
        };
        anchor
            .and_then(|anchor| ranked.iter().find(|hit| same(hit, anchor)))
            .or_else(|| ranked.first())
    }

    /// Adds what the services said in this lookup to the track's answer and saves it when one of
    /// them replied for the first time. A service that failed stays out of `answered`, so the
    /// next lookup asks it again.
    fn keep(
        &mut self,
        id: String,
        mut found: Found,
        hits: &[LyricsHit],
        answered: Vec<&'static str>,
        cx: &mut Context<Self>,
    ) {
        // A file's own lyrics are read again on every play, so they are never kept.
        if !self.online(&id, cx) {
            return;
        }
        let fresh: Vec<&'static str> = answered
            .into_iter()
            .filter(|name| *name != LOCAL && !found.answered.contains(name))
            .collect();
        if !fresh.is_empty() {
            found.hits.extend(
                hits.iter()
                    .filter(|hit| fresh.contains(&hit.source))
                    .cloned(),
            );
            found.answered.extend(fresh);
            if !found.hits.is_empty() {
                let key = self.key(&id, cx);
                self.unsaved
                    .push(Sheets::unsaved(key, &found.hits, &found.answered));
                self.schedule_save(cx);
            }
        }
        self.hold(id, found);
    }

    /// Puts up the answer once every source has replied. Unless a word-by-word sheet already
    /// settled the question, this is the best there is and nothing may replace it afterwards.
    fn settle(
        &mut self,
        ranked: Vec<LyricsHit>,
        displayed: Option<&LyricsHit>,
        instrumental: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.settled {
            log::debug!("lyrics: settling on the best answer");
            self.settled = true;
            self.pin(ranked, displayed);
            self.state = state_for(&self.hits, instrumental);
        }
        cx.notify();
        self.prefetch(cx);
    }

    fn pin(&mut self, ranked: Vec<LyricsHit>, displayed: Option<&LyricsHit>) {
        let anchor = match self.picked {
            true => self.hits.get(self.chosen).cloned(),
            false => displayed.cloned(),
        };
        let before = self.current().map(|hit| hit.lyrics.clone());
        let mut hits = ranked;
        let held = keep_displayed_first(&mut hits, anchor.as_ref());
        self.hits = hits;
        self.chosen = 0;
        self.picked &= held;
        if before.as_ref() != self.current().map(|hit| &hit.lyrics) {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Writes the unsaved answers once lookups have gone quiet for `SAVE_DELAY`. A write that
    /// has begun always finishes, even when another answer reschedules the save.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok((store, unsaved)) = this.update(cx, |this, _| {
                (this.store.clone(), std::mem::take(&mut this.unsaved))
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move { store.save(unsaved) })
                .detach();
        }));
    }
}

/// What a sheet is worth: word-by-word beats timed, timed beats untimed.
const WORDED: u8 = 3;
const TIMED: u8 = 2;

fn depth(lyrics: &Sheet) -> u8 {
    match (lyrics.worded(), lyrics.synced()) {
        (true, _) => WORDED,
        (false, true) => TIMED,
        (false, false) => 1,
    }
}

fn keep_displayed_first(hits: &mut Vec<LyricsHit>, displayed: Option<&LyricsHit>) -> bool {
    let Some(index) =
        displayed.and_then(|displayed| hits.iter().position(|hit| same(hit, displayed)))
    else {
        return false;
    };
    let displayed = hits.remove(index);
    hits.insert(0, displayed);
    true
}

fn same(left: &LyricsHit, right: &LyricsHit) -> bool {
    left.source == right.source
        && left.title == right.title
        && left.artist == right.artist
        && left.album == right.album
        && left.duration == right.duration
        && left.lyrics.worded() == right.lyrics.worded()
}

fn ordered(query: &LyricsQuery, hits: Vec<LyricsHit>) -> Vec<LyricsHit> {
    let mut ranked = music::lyrics::rank(query, hits);
    music::lyrics::reshape(&mut ranked);
    ranked
}

/// What the services have said about one track: every hit matched to it, unranked and from every
/// source, and the services that replied. The selection is applied when it is shown.
#[derive(Clone, Default)]
struct Found {
    hits: Vec<LyricsHit>,
    answered: Vec<&'static str>,
}

impl Found {
    /// Whether `provider` has nothing left to say. A file's own lyrics are never covered, since
    /// they are read again on every play.
    fn covers(&self, provider: &Arc<dyn LyricsProvider>) -> bool {
        provider.name() != LOCAL && self.answered.contains(&provider.name())
    }
}

/// Turns the sheet saved with an offline copy into a hit. `source` must be 'static, so a known
/// provider's name is reused, else it is labelled Subsonic.
fn offline_hit(kept: music::offline::CachedLyrics, known: &[&'static str]) -> LyricsHit {
    let source = known
        .iter()
        .copied()
        .find(|name| *name == kept.source)
        .unwrap_or("Subsonic");
    LyricsHit {
        source,
        trust: 0,
        lyrics: kept.lyrics,
        instrumental: kept.instrumental,
        title: kept.title,
        artist: kept.artist,
        album: None,
        duration: None,
        writers: Vec::new(),
    }
}

/// The sheet worth saving beside an offline copy: the best one that did not come from a file.
fn sheet_to_keep(ranked: &[LyricsHit]) -> Option<music::offline::CachedLyrics> {
    let best = ranked.iter().find(|hit| hit.source != LOCAL)?;
    Some(music::offline::CachedLyrics {
        source: best.source.to_owned(),
        lyrics: best.lyrics.clone(),
        instrumental: best.instrumental,
        title: best.title.clone(),
        artist: best.artist.clone(),
    })
}

fn state_for(hits: &[LyricsHit], instrumental: bool) -> LyricsState {
    match (hits.is_empty(), instrumental) {
        (false, _) => LyricsState::Ready,
        (true, true) => LyricsState::Instrumental,
        (true, false) => LyricsState::Missing,
    }
}

fn query_for(track: &Track, key: Option<TrackKey>) -> LyricsQuery {
    LyricsQuery {
        title: track.name.clone(),
        artist: track.artists.clone(),
        album: (!track.album.is_empty()).then(|| track.album.clone()),
        duration: track.duration,
        track: key,
    }
}

/// Asks every provider at once and sends each one's hits for the queried track as they arrive.
/// Returns the providers that replied, leaving out any that failed.
async fn gather(
    providers: Vec<Arc<dyn LyricsProvider>>,
    query: LyricsQuery,
    sender: tokio::sync::mpsc::UnboundedSender<Vec<LyricsHit>>,
) -> anyhow::Result<Vec<&'static str>> {
    let mut tasks = JoinSet::new();
    for provider in providers {
        let query = query.clone();
        tasks.spawn(async move {
            let name = provider.name();
            let mut found = provider
                .search(&query)
                .await
                .inspect_err(|error| log::warn!("lyrics: {name} did not answer: {error:#}"))
                .ok()?;
            // Hits for another track are dropped here, before the lookup holds and copies them.
            found.retain(|hit| music::lyrics::matched(&query, hit));
            Some((name, found))
        });
    }
    let mut answered = Vec::new();
    while !tasks.is_empty() {
        tokio::select! {
            // Dropping the foreground lookup (including on a provider change) must also
            // stop its network requests. Dropping this JoinSet aborts the remaining tasks.
            _ = sender.closed() => break,
            found = tasks.join_next() => {
                if let Some(Ok(Some((name, found)))) = found {
                    answered.push(name);
                    sender.send(found).ok();
                }
            }
        }
    }
    Ok(answered)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use music::{Lyrics as Sheet, LyricsHit, LyricsLine, LyricsWord, Voice};

    use super::{LOCAL, keep_displayed_first, offline_hit, sheet_to_keep};

    #[test]
    fn offline_sheet_keeps_a_known_source_and_labels_others_subsonic() {
        let kept = music::offline::CachedLyrics {
            source: "LRCLIB".to_owned(),
            lyrics: Sheet::plain("line"),
            instrumental: false,
            title: "t".to_owned(),
            artist: "a".to_owned(),
        };
        let known = ["LRCLIB", "Musixmatch"];
        let shown = offline_hit(kept.clone(), &known);
        assert_eq!(shown.source, "LRCLIB");
        assert_eq!(shown.lyrics, Sheet::plain("line"));
        let unknown = music::offline::CachedLyrics {
            source: "Gone".to_owned(),
            ..kept
        };
        assert_eq!(offline_hit(unknown, &known).source, "Subsonic");
    }

    #[test]
    fn the_offline_copy_keeps_the_best_non_file_sheet() {
        let ranked = vec![
            hit(LOCAL, Sheet::plain("file")),
            hit("LRCLIB", Sheet::plain("web")),
        ];
        let kept = sheet_to_keep(&ranked).expect("a service sheet");
        assert_eq!(kept.source, "LRCLIB");
        assert_eq!(kept.lyrics, Sheet::plain("web"));
        assert!(sheet_to_keep(&ranked[..1]).is_none());
        assert!(sheet_to_keep(&[]).is_none());
    }

    fn hit(source: &'static str, lyrics: Sheet) -> LyricsHit {
        LyricsHit {
            source,
            trust: 0,
            lyrics,
            instrumental: false,
            title: "title".to_owned(),
            artist: "artist".to_owned(),
            album: None,
            duration: None,
            writers: Vec::new(),
        }
    }

    #[test]
    fn a_displayed_karaoke_result_stays_selected_after_final_ranking() {
        let plain = hit("plain", Sheet::plain("line"));
        let displayed = hit(
            "karaoke",
            Sheet::Synced {
                lines: vec![LyricsLine {
                    start: Duration::ZERO,
                    end: Some(Duration::from_secs(1)),
                    text: "line".to_owned(),
                    romanized: None,
                    words: Some(vec![LyricsWord {
                        start: Duration::ZERO,
                        end: Duration::from_secs(1),
                        text: "line".to_owned(),
                    }]),
                    secondary: Vec::new(),
                    voice: Voice::Lead,
                }]
                .into(),
            },
        );
        let mut final_ranking = vec![plain, displayed.clone()];

        keep_displayed_first(&mut final_ranking, Some(&displayed));

        assert_eq!(final_ranking.first(), Some(&displayed));
    }
}
