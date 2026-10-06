//! The page an artist link opens while the network is gone: everything of that artist that
//! is saved offline, read from the offline index alone, so every album and song plays.

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Pixels, Render, ScrollHandle, SharedString,
    Window, div,
};
use i18n::t;
use music::Track;
use state::{Offline, OfflineAlbum, Playback};
use ui::{
    ActiveTheme as _, Card, Listing as _, Scrollbar, Scroller, TableDelegate, TableEvent,
    TableState, Text, runtime, table, vacant,
};

use crate::shared::album_grid::CardGrid;
use crate::shared::cells;
use crate::shared::hero::{HeroMetaStrip, HeroPlayButton, PageHero};
use crate::shared::page;
use crate::shared::tracks::{TrackSource, Tracks, artist_columns};

/// The saved songs of the artist the page shows, in album order.
struct ArtistTracks(Entity<Offline>);

impl Tracks for ArtistTracks {
    fn tracks<'a>(&self, cx: &'a App) -> &'a [Track] {
        self.0.read(cx).artist_listed()
    }

    fn is_loading(&self, _cx: &App) -> bool {
        false
    }
}

/// The offline artist page: a header naming the artist, its saved albums as cards that play
/// from disk, and every saved song of it in one table.
pub(crate) struct OfflineArtistView {
    offline: Entity<Offline>,
    playback: Entity<Playback>,
    width: Pixels,
    scrollbar: Entity<Scrollbar>,
    table: Entity<TableState<TrackSource>>,
}

impl OfflineArtistView {
    pub(crate) fn new(
        offline: Entity<Offline>,
        playback: Entity<Playback>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let width = cells::content_width(window, Pixels::ZERO, cx);
        let id = cx.entity_id();
        let scrollbar = cx.new(|_| Scrollbar::new(ScrollHandle::new()).watching(id));
        let scroll = scrollbar.read(cx).scroll().clone();
        let table = cx.new(|cx| {
            let menu = cx.new(|_| Scrollbar::inset().watching(id));
            let source = TrackSource::new(
                artist_columns(false),
                ArtistTracks(offline.clone()),
                playback.clone(),
                menu,
                cx,
            )
            .table(cx.weak_entity());
            TableState::new(TableDelegate::new(source, width, cx), cx).follow(scroll)
        });

        cx.observe(&offline, |this, _, cx| {
            this.table.rebuild(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&playback, |this, _, cx| {
            this.table.refresh(cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(&table, |this, _, event, cx| match event {
            TableEvent::DoubleClicked(display) => {
                page::play(&this.table, &this.playback, *display, cx);
            }
            TableEvent::Activated(display) => {
                page::play_or_toggle(&this.table, &this.playback, *display, cx);
            }
            _ => {}
        })
        .detach();

        Self {
            offline,
            playback,
            width,
            scrollbar,
            table,
        }
    }

    fn header(&self, cx: &App) -> Option<AnyElement> {
        let offline = self.offline.read(cx);
        let artist = offline.artist()?;
        let duration: std::time::Duration = artist.tracks.iter().map(|track| track.duration).sum();
        let mut strip = HeroMetaStrip::new();
        if !artist.albums.is_empty() {
            strip = strip.text(t!(
                "offline-artist-album-count",
                count = artist.albums.len()
            ));
        }
        strip = strip.text(t!("count-songs", count = artist.tracks.len()));
        if !duration.is_zero() {
            strip = strip.text(runtime(duration));
        }
        let cover = artist
            .albums
            .iter()
            .find_map(|album| album.cover.clone())
            .or_else(|| artist.tracks.iter().find_map(|track| track.cover.clone()));
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(HeroPlayButton::listed(
                "play-offline-artist",
                t!("artist-play"),
                &self.table,
                self.playback.clone(),
            ))
            .child(HeroPlayButton::shuffle_listed(
                "shuffle-offline-artist",
                &self.table,
                self.playback.clone(),
            ));

        Some(
            PageHero::new(
                "offline-artist-hero",
                SharedString::from(artist.name.clone()),
            )
            .cover(cover)
            .fallback("icons/user.svg")
            .eyebrow(t!("offline-artist-eyebrow"))
            .meta(strip)
            .actions(actions)
            .circle()
            .into_any_element(),
        )
    }

    fn albums(&self, available: Pixels, cx: &App) -> Option<AnyElement> {
        let theme = *cx.theme();
        let albums = self.offline.read(cx).artist()?.albums.clone();
        if albums.is_empty() {
            return None;
        }
        let layout = CardGrid::layout(available);
        let cards = albums.into_iter().enumerate().map(|(index, album)| {
            album_card(index, album, &self.playback, layout.card, cx).into_any_element()
        });

        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .pt_6()
                .child(
                    div()
                        .text_size(theme.text(Text::Title))
                        .font_weight(FontWeight::BOLD)
                        .child(t!("offline-artist-albums")),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .w_full()
                        .gap_x(layout.gap)
                        .gap_y_6()
                        .children(cards),
                )
                .into_any_element(),
        )
    }

    fn songs_title(&self, cx: &App) -> Option<AnyElement> {
        let theme = *cx.theme();
        self.offline.read(cx).artist()?;
        Some(
            div()
                .pt_6()
                .pb_3()
                .text_size(theme.text(Text::Title))
                .font_weight(FontWeight::BOLD)
                .child(t!("offline-artist-songs"))
                .into_any_element(),
        )
    }
}

impl Render for OfflineArtistView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.table.claim(cx);
        let inset = cx.theme().metrics.inset;
        let width = cells::content_width(window, Pixels::ZERO, cx);
        if (width - self.width).abs() >= gpui::px(0.5) {
            self.width = width;
            self.table.set_width(width, cx);
        }

        let scroll = self.scrollbar.read(cx).scroll().clone();
        let viewport = page::viewport(&scroll, inset, window);
        self.table
            .update(cx, |table, _| table.set_viewport(viewport));

        let header = self.header(cx);
        let albums = self.albums(width - inset * 2., cx);
        let songs = self.songs_title(cx);
        let gone = header.is_none();
        let page = Scroller::new("offline-artist-page", &self.scrollbar)
            .pt(inset)
            .pb(inset)
            .child(
                div()
                    .px(inset)
                    .children(header)
                    .children(albums)
                    .children(songs),
            )
            .child(table(&self.table))
            .when(gone, |this| {
                this.child(vacant(t!("offline-artist-empty"), cx))
            });

        div().size_full().child(page)
    }
}

/// A saved album as a card: its play control and a press both play the album's saved tracks
/// from disk, since the album page itself needs the network.
fn album_card(
    index: usize,
    album: OfflineAlbum,
    playback: &Entity<Playback>,
    width: Pixels,
    cx: &App,
) -> Card {
    let current = {
        let playback = playback.read(cx);
        let playing = playback.track().and_then(|track| track.id.as_deref());
        let held = playing.is_some_and(|id| {
            album
                .tracks
                .iter()
                .any(|track| track.id.as_deref() == Some(id))
        });
        held.then(|| playback.control() == Some(true))
    };
    let count = album.tracks.len();
    let toggled = playback.clone();
    let pressed = playback.clone();
    let queued = album.tracks.clone();
    let started = album.tracks;

    Card::new(
        ("offline-artist-album", index),
        SharedString::from(album.name),
    )
    .cover(album.cover)
    .fallback("icons/disc-3.svg")
    .weight(FontWeight::SEMIBOLD)
    .meta(t!("count-songs", count = count))
    .tile(width)
    .flat()
    .play(current == Some(true), move |_, _, cx| {
        toggled.update(cx, |playback, cx| match current {
            Some(_) => playback.toggle_play(cx),
            None => playback.start_any(queued.clone(), None, cx),
        });
    })
    .press(move |_, _, cx| {
        pressed.update(cx, |playback, cx| {
            playback.start(started.clone(), 0, None, cx)
        });
    })
}
