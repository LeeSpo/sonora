use std::cmp::Ordering;
use ui::{ActiveTheme as _, Filter, FilterChange, FlagAxis};

use gpui::{AnyElement, App, Entity, SharedString};
use i18n::t;
use music::{SavedArtist, Shape};
use state::{Library, LibraryPart, Network, Origin, Playback, Shelf};
use ui::rank::{ESSENTIAL, HANDY};
use ui::{Cell, ColumnSpec, Menu, Pin, TableSource, Width};

use crate::shared::cells::{self, DATE, NUMBER};
use crate::shared::menus::artist_menu;
use crate::shared::pins::Pinned as _;
use crate::shared::text::{folded, holds};
use crate::shared::tracks::initial;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ArtistField {
    Index,
    Cover,
    Name,
    AddedAt,
}

const COLUMN: ColumnSpec<ArtistField> = ColumnSpec::filling(ArtistField::Index);

const INDEX: ColumnSpec<ArtistField> = ColumnSpec::numbering(ArtistField::Index, NUMBER);

const COVER: ColumnSpec<ArtistField> = ColumnSpec::artwork(ArtistField::Cover);

const NAME: ColumnSpec<ArtistField> = ColumnSpec {
    field: ArtistField::Name,
    key: "name",
    header: "column-name",
    rank: ESSENTIAL,
    ..COLUMN
};

const ADDED_AT: ColumnSpec<ArtistField> = ColumnSpec {
    field: ArtistField::AddedAt,
    key: "added-at",
    header: "column-date-added",
    width: Width::Fixed(DATE),
    rank: HANDY,
    ..COLUMN
};

pub(super) const COLUMNS: &[ColumnSpec<ArtistField>] = &[INDEX, COVER, NAME, ADDED_AT];

pub(super) struct ArtistSource {
    library: Entity<Library>,
    playback: Entity<Playback>,
    shelf: Shelf,
    starred: bool,
    /// When set, starred stays on and is omitted from the funnel (Favorites page).
    favorites_only: bool,
}

impl ArtistSource {
    pub(super) fn shelved(
        library: Entity<Library>,
        playback: Entity<Playback>,
        shelf: Shelf,
    ) -> Self {
        Self {
            library,
            playback,
            shelf,
            starred: false,
            favorites_only: false,
        }
    }

    pub(super) fn favorites_only(mut self) -> Self {
        self.favorites_only = true;
        self.starred = true;
        self
    }

    fn index_cell(&self, cell: &Cell<ArtistField>, artist: &SavedArtist, cx: &App) -> AnyElement {
        let origin = Origin::artist(artist.id.clone()).named(artist.name.clone());
        let playing = self.playback.read(cx).playing_from(&origin);
        let played = origin.clone();
        let press = cells::toggle(&self.playback, playing, move |playback, cx| {
            playback.play_origin(played.clone(), cx)
        });

        cells::index(cell, playing, true, None, None, press, cx)
    }

    pub(super) fn at(&self, row: usize, cx: &App) -> Option<SavedArtist> {
        self.artists(cx).get(row).cloned()
    }

    fn artists<'a>(&self, cx: &'a App) -> &'a [SavedArtist] {
        let library = self.library.read(cx);
        match self.favorites_only {
            true => library.favorite_artists(self.shelf),
            false => library.state(self.shelf).artists(),
        }
    }

    /// Whether the shelf lists more than the favorites, so a favorites filter has something to do.
    fn catalog(&self, cx: &App) -> bool {
        self.library.read(cx).shape(self.shelf) == Shape::Catalog
    }
}

impl TableSource for ArtistSource {
    type Field = ArtistField;

    fn columns(&self) -> &'static [ColumnSpec<ArtistField>] {
        COLUMNS
    }

    fn rows(&self, cx: &App) -> usize {
        self.artists(cx).len()
    }

    fn matches(&self, row: usize, query: &str, cx: &App) -> bool {
        self.at(row, cx).is_some_and(|artist| {
            if self.starred
                && !self.favorites_only
                && !self.library.read(cx).saved_artist(&artist.id)
            {
                return false;
            }
            // Offline Favorites: only artists that open the offline artist page.
            if self.favorites_only
                && Network::lost(cx)
                && !crate::shared::offline_play::artist_cached(&artist.id, cx)
            {
                return false;
            }
            holds(&artist.name, query)
        })
    }

    fn filter_axes(&self, cx: &App) -> Vec<Filter> {
        match self.catalog(cx) && !self.favorites_only {
            true => vec![Filter::Flag(FlagAxis {
                key: "filter-favorites",
                label: t!("filter-favorites"),
                on: self.starred,
            })],
            false => Vec::new(),
        }
    }

    fn filter(&mut self, change: FilterChange, _cx: &App) -> bool {
        match change {
            FilterChange::Flag("filter-favorites", value) if !self.favorites_only => {
                self.starred = value;
                true
            }
            FilterChange::Reset => {
                self.starred = self.favorites_only;
                true
            }
            _ => false,
        }
    }

    fn filtered(&self, _cx: &App) -> bool {
        self.starred && !self.favorites_only
    }

    fn playing(&self, row: usize, cx: &App) -> bool {
        self.artists(cx).get(row).is_some_and(|artist| {
            let origin = Origin::artist(artist.id.clone());
            self.playback.read(cx).playing_from(&origin).is_some()
        })
    }

    fn is_loading(&self, cx: &App) -> bool {
        let library = self.library.read(cx);
        match self.favorites_only {
            true => library.favorites_loading(self.shelf),
            false => library.loading(self.shelf, LibraryPart::Artists),
        }
    }

    fn pin(&self, row: usize, cx: &App) -> Option<Pin> {
        self.artists(cx).get(row)?.pin()
    }

    fn picking(&self) -> bool {
        true
    }

    fn context_menu(&self, rows: &[usize], _visible: &[ArtistField], cx: &App) -> Option<Menu> {
        Some(artist_menu(
            self.at(*rows.first()?, cx)?,
            self.playback.clone(),
            false,
            cx,
        ))
    }

    fn cell(&self, cell: Cell<ArtistField>, cx: &mut App) -> AnyElement {
        let theme = *cx.theme();
        let muted = theme.muted_foreground;

        let Some(artist) = self.artists(cx).get(cell.row) else {
            return cells::blank(&cell);
        };

        if cell.field == ArtistField::Index {
            return self.index_cell(&cell, artist, cx);
        }

        match cell.field {
            ArtistField::Cover => cells::avatar(&cell, artist.cover.clone()),
            ArtistField::Name => cells::dim(&cell, artist.name.clone(), theme.foreground),
            ArtistField::AddedAt => cells::dim(&cell, cells::stamp(artist.added_at), muted),
            ArtistField::Index => cells::blank(&cell),
        }
    }

    fn compare(&self, field: ArtistField, a: usize, b: usize, cx: &App) -> Ordering {
        let artists = self.artists(cx);
        let at = |index: usize| artists.get(index);

        match field {
            ArtistField::Name => folded(
                at(a).map(|artist| artist.name.as_str()).unwrap_or_default(),
                at(b).map(|artist| artist.name.as_str()).unwrap_or_default(),
            ),
            ArtistField::AddedAt => at(a)
                .map(|artist| artist.added_at)
                .cmp(&at(b).map(|artist| artist.added_at)),
            ArtistField::Index | ArtistField::Cover => a.cmp(&b),
        }
    }

    fn group(&self, field: ArtistField, row: usize, cx: &App) -> Option<SharedString> {
        let artist = self.artists(cx).get(row)?;

        match field {
            ArtistField::Name => Some(initial(&artist.name)),
            _ => None,
        }
    }
}
