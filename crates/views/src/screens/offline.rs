use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Pixels, Render, ScrollHandle, SharedString,
    Window, div,
};
use i18n::t;
use music::Track;
use state::{Offline, Playback};
use ui::{
    ActiveTheme as _, Button, Card, Listing as _, Scrollbar, Scroller, TableDelegate, TableEvent,
    TableState, Text, runtime, table, vacant,
};

use crate::chrome::{Searchable, Toolbar, Tooled};
use crate::shared::album_grid::CardGrid;
use crate::shared::cells;
use crate::shared::confirm::Confirm;
use crate::shared::hero::{HeroMetaStrip, PageHero};
use crate::shared::page;
use crate::shared::tracks::{LIBRARY_COLUMNS, TrackSource, Tracks};

struct OfflineTracks(Entity<Offline>);

impl Tracks for OfflineTracks {
    fn tracks<'a>(&self, cx: &'a App) -> &'a [Track] {
        self.0.read(cx).listed()
    }

    fn is_loading(&self, _cx: &App) -> bool {
        false
    }
}

pub(crate) struct OfflineView {
    offline: Entity<Offline>,
    playback: Entity<Playback>,
    width: Pixels,
    scrollbar: Entity<Scrollbar>,
    table: Entity<TableState<TrackSource>>,
    toolbar: Entity<Toolbar>,
}

impl OfflineView {
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
                LIBRARY_COLUMNS,
                OfflineTracks(offline.clone()),
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

        let toolbar = Toolbar::searchable(&cx.entity(), cx);
        Self {
            offline,
            playback,
            width,
            scrollbar,
            table,
            toolbar,
        }
    }

    fn note(&self, cx: &App) -> Option<SharedString> {
        if self.table.row_count(cx) > 0 {
            return None;
        }
        if self.table.filtering(cx) {
            return Some(t!("library-no-matches"));
        }
        Some(t!("offline-empty"))
    }

    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let (count, duration, usage) = {
            let offline = self.offline.read(cx);
            let tracks = offline.tracks();
            let duration: std::time::Duration = tracks.iter().map(|track| track.duration).sum();
            (tracks.len(), duration, offline.usage())
        };
        let (progress, failed) = {
            let offline = self.offline.read(cx);
            (offline.progress(), offline.failed_count())
        };
        // Glanceable: "128 songs · 3.2 GB"
        let mut strip = HeroMetaStrip::new().text(t!("count-songs", count = count));
        if usage.bytes > 0 {
            strip = strip.text(SharedString::from(music::offline::format_bytes(usage.bytes)));
        }
        if !duration.is_zero() {
            strip = strip.text(runtime(duration));
        }
        if let Some((settled, total)) = progress {
            strip = strip.text(t!("offline-progress", settled = settled, total = total));
        }
        if failed > 0 {
            strip = strip.text(t!("offline-failed-count", count = failed));
        }

        let mut actions = div().flex().items_center().gap_2();
        if count > 0 {
            actions = actions.child(
                Button::new("clear-all-offline")
                    .outline()
                    .icon("icons/trash-2.svg")
                    .label(t!("offline-clear-all"))
                    .on_click(|_, _, cx| Confirm::offline_all(cx)),
            );
        }
        if failed > 0 {
            actions = actions
                .child(
                    Button::new("retry-offline-failed")
                        .outline()
                        .icon("icons/refresh-cw.svg")
                        .label(t!("offline-retry-failed"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.offline
                                .update(cx, |offline, cx| offline.retry(None, cx));
                        })),
                )
                .child(
                    Button::new("clear-offline-failed")
                        .ghost()
                        .icon("icons/x.svg")
                        .label(t!("offline-clear-failed"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.offline
                                .update(cx, |offline, cx| offline.dismiss_failed(None, cx));
                        })),
                );
        }

        PageHero::new("offline-hero", t!("nav-offline"))
            .fallback("icons/download.svg")
            .accent()
            .eyebrow(t!("detail-playlist"))
            .meta(strip)
            .when(count > 0 || failed > 0, |hero| hero.actions(actions))
            .into_any_element()
    }

    fn albums(&self, available: Pixels, cx: &App) -> Option<AnyElement> {
        let theme = *cx.theme();
        let albums = self.offline.read(cx).albums();
        if albums.is_empty() {
            return None;
        }
        let layout = CardGrid::layout(available);
        let cards = albums.into_iter().enumerate().map(|(index, album)| {
            let count = album.track_count;
            let size = music::offline::format_bytes(album.bytes);
            let meta = t!(
                "offline-album-meta",
                count = count,
                size = size.as_str()
            );
            let album_id = album.album_id.clone();
            let album_name = album.name.clone();
            let clear_id = album_id.clone();
            let clear_name = album_name.clone();
            Card::new(
                ("offline-album", index),
                SharedString::from(album.name),
            )
            .cover(album.cover)
            .fallback("icons/disc-3.svg")
            .weight(FontWeight::SEMIBOLD)
            .meta(meta)
            .tile(layout.card)
            .flat()
            .trailing(
                Button::new(("clear-offline-album", index))
                    .ghost()
                    .icon("icons/trash-2.svg")
                    .tooltip("offline-clear-album")
                    .on_click(move |_, _, cx| {
                        Confirm::offline_album(clear_id.clone(), clear_name.clone(), cx);
                    }),
            )
            .into_any_element()
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
                        .child(t!("offline-albums")),
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
        if self.offline.read(cx).tracks().is_empty() {
            return None;
        }
        let theme = *cx.theme();
        Some(
            div()
                .pt_6()
                .pb_3()
                .text_size(theme.text(Text::Title))
                .font_weight(FontWeight::BOLD)
                .child(t!("offline-songs"))
                .into_any_element(),
        )
    }
}

impl Render for OfflineView {
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

        let note = self.note(cx);
        let albums = self.albums(width - inset * 2., cx);
        let songs = self.songs_title(cx);
        let page = Scroller::new("offline-page", &self.scrollbar)
            .pt(inset)
            .pb(inset)
            .child(div().px(inset).child(self.header(cx)))
            .child(div().px(inset).children(albums).children(songs))
            .child(table(&self.table))
            .when_some(note, |this, note| this.child(vacant(note, cx)));

        div().size_full().child(page)
    }
}

impl Searchable for OfflineView {
    fn search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.table.set_query(query, cx);
        cx.notify();
    }

    fn hint() -> SharedString {
        "filter-offline".into()
    }
}

impl Tooled for OfflineView {
    fn toolbar(&self) -> Entity<Toolbar> {
        self.toolbar.clone()
    }

    fn tools(&self, _cx: &App) -> Vec<AnyElement> {
        Vec::new()
    }
}
