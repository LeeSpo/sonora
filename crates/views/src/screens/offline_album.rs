//! The page a saved album opens while the network is gone (or from the Downloaded page): the
//! album's saved songs, read from the offline index alone. A song plays from itself with the
//! album's saved songs as the queue; songs that are not on disk are not listed.

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Pixels, Render, ScrollHandle, SharedString, Window, div,
};
use i18n::t;
use music::Track;
use state::{Offline, Playback};
use ui::{
    ActiveTheme as _, Listing as _, Scrollbar, Scroller, TableDelegate, TableEvent, TableState,
    runtime, table, vacant,
};

use crate::shared::cells;
use crate::shared::hero::{HeroMetaStrip, HeroPlayButton, PageHero};
use crate::shared::page;
use crate::shared::tracks::{TrackSource, Tracks, album_columns};

/// The saved songs of the album the page shows, in disc and track order.
struct AlbumTracks(Entity<Offline>);

impl Tracks for AlbumTracks {
    fn tracks<'a>(&self, cx: &'a App) -> &'a [Track] {
        self.0.read(cx).album_listed()
    }

    fn is_loading(&self, _cx: &App) -> bool {
        false
    }
}

pub(crate) struct OfflineAlbumView {
    offline: Entity<Offline>,
    playback: Entity<Playback>,
    width: Pixels,
    scrollbar: Entity<Scrollbar>,
    table: Entity<TableState<TrackSource>>,
}

impl OfflineAlbumView {
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
                album_columns(false),
                AlbumTracks(offline.clone()),
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
        // A click plays from that song with the album's saved songs as the queue.
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
        let album = self.offline.read(cx).album()?;
        let duration: std::time::Duration = album.tracks.iter().map(|track| track.duration).sum();
        let artists = album
            .tracks
            .iter()
            .map(|track| track.artists.clone())
            .find(|artists| !artists.is_empty());
        let mut strip = HeroMetaStrip::new();
        if let Some(artists) = artists {
            strip = strip.text(SharedString::from(artists));
        }
        strip = strip.text(t!("count-songs", count = album.tracks.len()));
        if !duration.is_zero() {
            strip = strip.text(runtime(duration));
        }
        strip = strip.text(SharedString::from(music::offline::format_bytes(
            album.bytes,
        )));
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(HeroPlayButton::listed(
                "play-offline-album",
                t!("artist-play"),
                &self.table,
                self.playback.clone(),
            ))
            .child(HeroPlayButton::shuffle_listed(
                "shuffle-offline-album",
                &self.table,
                self.playback.clone(),
            ));

        Some(
            PageHero::new("offline-album-hero", SharedString::from(album.name.clone()))
                .cover(album.cover.clone())
                .fallback("icons/disc-3.svg")
                .eyebrow(t!("nav-offline"))
                .meta(strip)
                .actions(actions)
                .into_any_element(),
        )
    }
}

impl Render for OfflineAlbumView {
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
        let gone = header.is_none();
        // One child above the table: `page::viewport` measures only item 0 (see offline.rs).
        let page = Scroller::new("offline-album-page", &self.scrollbar)
            .pt(inset)
            .pb(inset)
            .child(div().px(inset).pb_6().children(header))
            .child(table(&self.table))
            .when(gone, |this| this.child(vacant(t!("offline-empty"), cx)));

        div().size_full().child(page)
    }
}
