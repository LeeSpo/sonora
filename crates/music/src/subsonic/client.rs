use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use opensubsonic::api::lists::AlbumListType;
use opensubsonic::data::{AlbumId3, AlbumWithSongsId3, Child, Genre as SourceGenre};
use opensubsonic::{Auth, Client};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;

use crate::engine::Loudness;
use crate::escape;
use crate::subsonic::auth::Signature;
use crate::subsonic::wire;
use crate::{
    Album, AlbumCatalogue, AlbumDetail, Artist, ArtistCatalogue, ArtistProfile, Genre, GenreDetail,
    GenreItem, GenreSection, HomeFeed, MediaKind, MusicApi, Playlist, PlaylistDetail, Report,
    SUGGESTIONS, SavedArtist, Track, UserProfile, distinct_covers,
};

const PORTRAIT_LIMIT: usize = 24;
const RADIO_COUNT: i32 = 25;
/// How many songs a name search offers as the artist's own, and how many of the most played
/// of them the artist page lists.
const ARTIST_SONGS: i32 = 200;
const TOP_TRACKS: usize = 20;
/// How long the artist's top tracks may take. The search is answered from the server's own
/// library, so this only bounds a server that has stopped answering.
const LOCAL_WAIT: Duration = Duration::from_secs(3);
const HOME_SONGS: i32 = 25;
const HOME_ALBUMS: i32 = 12;
const LIBRARY_PAGE: i32 = 500;
const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "sonora";
/// The OpenSubsonic extension that takes a playback position and state.
const PLAYBACK_REPORT: &str = "playbackReport";

#[derive(Clone)]
pub struct SubsonicClient {
    client: Client,
    /// Active base URL, kept so a failed call can invalidate the multi-address cache.
    server: String,
    username: String,
    http: reqwest::Client,
    /// The cover art endpoint with the fixed signature already on it. The library client signs
    /// every url afresh, which would give one cover a new url on every conversion and defeat
    /// every image cache between here and the screen.
    covers: String,
    /// The download endpoint with the fixed signature already on it, for streaming originals
    /// to the offline cache without buffering the whole body in RAM.
    downloads: String,
    /// Whether the server takes `reportPlayback`, asked the first time a report goes out.
    playback_report: Arc<OnceCell<bool>>,
}

/// What the server records about a track that playback wants before the decoder can tell.
#[derive(Clone, Copy, Debug, Default)]
pub struct Details {
    pub duration: Option<Duration>,
    /// The track's ReplayGain, which OpenSubsonic servers such as Navidrome report and plain
    /// Subsonic servers do not.
    pub loudness: Option<Loudness>,
}

impl SubsonicClient {
    pub fn new(
        server: String,
        username: String,
        password: String,
        signature: &Signature,
    ) -> Result<Self> {
        let server = server.trim_end_matches('/').to_owned();
        let client = Client::new(&server, Auth::token(&username, password))
            .context("cannot parse the subsonic server address")?
            .with_client_name(CLIENT_NAME);
        let auth = format!(
            "u={}&t={}&s={}&v={API_VERSION}&c={CLIENT_NAME}&f=json",
            escape::component(&username),
            escape::component(&signature.token),
            escape::component(&signature.salt),
        );
        let covers = format!("{server}/rest/getCoverArt?{auth}");
        let downloads = format!("{server}/rest/download?{auth}");
        Ok(Self {
            client,
            server,
            username,
            http: reqwest::Client::new(),
            covers,
            downloads,
            playback_report: Arc::default(),
        })
    }

    /// Whether the server lists the `playbackReport` extension, asked once per client. A plain
    /// Subsonic server has no extension list and answers with an error, which counts as no.
    async fn reports_playback(&self) -> bool {
        *self
            .playback_report
            .get_or_init(|| async {
                match self.client.get_open_subsonic_extensions().await {
                    Ok(extensions) => extensions
                        .iter()
                        .any(|extension| extension.name == PLAYBACK_REPORT),
                    Err(error) => {
                        log::info!("subsonic: the server lists no extensions: {error:#}");
                        false
                    }
                }
            })
            .await
    }

    fn cover_url(&self, id: &str, size: i32) -> Option<String> {
        Some(format!(
            "{}&id={}&size={size}",
            self.covers,
            escape::component(id)
        ))
    }

    fn cover(&self, art: Option<&str>, fallback: &str) -> Option<String> {
        self.cover_url(art.filter(|id| !id.is_empty()).unwrap_or(fallback), 300)
    }

    fn cover_large(&self, art: Option<&str>, fallback: &str) -> Option<String> {
        self.cover_url(art.filter(|id| !id.is_empty()).unwrap_or(fallback), 600)
    }

    fn song(&self, song: Child) -> Track {
        let cover = self.cover(song.cover_art.as_deref(), &song.id);
        wire::track(song, cover)
    }

    fn convert_album(&self, source: AlbumId3) -> Album {
        let cover = self.cover(source.cover_art.as_deref(), &source.id);
        let large = self.cover_large(source.cover_art.as_deref(), &source.id);
        wire::album(source, cover, large)
    }

    fn detail_album(&self, detail: &AlbumWithSongsId3, tracks: usize) -> Album {
        let id = detail.id.clone();
        let cover = self.cover(detail.cover_art.as_deref(), &id);
        let large = self.cover_large(detail.cover_art.as_deref(), &id);
        let (artists, artist_refs) = wire::artists_of(
            detail.artist.clone(),
            detail.artist_id.clone(),
            detail.artists.as_ref(),
            detail.display_artist.clone(),
        );
        let year = detail.year.unwrap_or(0);
        Album {
            id,
            name: detail.name.clone(),
            artists,
            artist_refs,
            cover,
            cover_large: large,
            release_type: wire::release_type(
                detail.release_types.as_deref(),
                detail.is_compilation,
            ),
            year,
            track_count: detail
                .song_count
                .map(|count| count.max(0) as u32)
                .unwrap_or(tracks as u32),
            release_date: match year {
                0 => String::new(),
                _ => year.to_string(),
            },
            label: wire::labels(detail.record_labels.as_deref()),
            copyrights: Vec::new(),
            added_at: None,
        }
    }

    fn convert_playlist(&self, source: &opensubsonic::data::Playlist) -> Playlist {
        let cover = source
            .cover_art
            .as_deref()
            .and_then(|id| self.cover_url(id, 300));
        wire::playlist(
            &source.id,
            &source.name,
            source.owner.as_deref(),
            source.public.unwrap_or(false),
            source.song_count.unwrap_or(0).max(0) as u32,
            cover,
            &self.username,
        )
    }

    /// The artist's picture from the server's own cover art endpoint. `artistImageUrl` is
    /// left alone on purpose: Navidrome fills it with a Last.fm or Spotify address, which
    /// would have the app fetch every portrait from outside the server.
    fn artist_cover(&self, art: Option<&str>, fallback: &str) -> Option<String> {
        self.cover_large(art, fallback)
    }

    /// The artist as `getArtist` answers it: name, picture and albums, all from the server's
    /// own library. The biography and the top songs are left out, because Navidrome asks
    /// Last.fm or Spotify for both.
    async fn artist_detail(&self, artist_id: &str) -> Result<Artist> {
        let detail = self
            .client
            .get_artist(artist_id)
            .await
            .context("cannot load the artist")?;
        let cover_large = self.artist_cover(detail.cover_art.as_deref(), artist_id);
        let albums = detail
            .album
            .into_iter()
            .map(|album| self.convert_album(album))
            .collect();
        Ok(Artist {
            name: detail.name,
            cover_large,
            biography: None,
            monthly_listeners: None,
            top_tracks: Vec::new(),
            albums,
        })
    }

    /// The artist's songs, from a name search the server answers out of its own library.
    /// The search also finds other artists' songs that mention the name, so only songs
    /// credited to the artist are kept.
    async fn artist_songs(&self, artist_id: &str, name: &str) -> Result<Vec<Track>> {
        let search = self.client.search3(
            name,
            Some(0),
            None,
            Some(0),
            None,
            Some(ARTIST_SONGS),
            None,
            None,
        );
        let found = tokio::time::timeout(LOCAL_WAIT, search)
            .await
            .with_context(|| format!("the songs of artist {artist_id} took too long"))?
            .with_context(|| format!("cannot search the songs of artist {artist_id}"))?;
        let songs = found.song.into_iter().map(|song| self.song(song)).collect();
        Ok(credited(artist_id, name, songs))
    }

    /// Up to `SUGGESTIONS` of the artist's own albums without the album the page is already
    /// showing.
    async fn more_from_artist(&self, album_id: &str, artist_id: &str) -> Result<Vec<Album>> {
        let detail = self
            .client
            .get_artist(artist_id)
            .await
            .with_context(|| format!("cannot load more from artist {artist_id}"))?;
        Ok(detail
            .album
            .into_iter()
            .map(|album| self.convert_album(album))
            .filter(|album| album.id != album_id)
            .take(SUGGESTIONS)
            .collect())
    }
}

#[async_trait]
impl MusicApi for SubsonicClient {
    fn share_url(&self, _kind: MediaKind, _id: &str) -> Option<String> {
        None
    }

    async fn profile(&self) -> Result<UserProfile> {
        match self.client.ping().await {
            Ok(()) => Ok(wire::profile(self.username.clone())),
            Err(error) => {
                let reason = format!("{error:#}");
                if crate::trouble::offline(&reason) {
                    crate::subsonic::connection::invalidate(Some(&self.server));
                }
                Err(error).context("cannot reach the subsonic server")
            }
        }
    }

    /// The artist with the songs to play for it: the most played ones, or the artist's
    /// songs as the search finds them when none has been played yet.
    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        let mut artist = self.artist_detail(artist_id).await?;
        let songs = match self.artist_songs(artist_id, &artist.name).await {
            Ok(songs) => songs,
            Err(error) => {
                log::warn!("subsonic: {error:#}");
                Vec::new()
            }
        };
        let top = most_played(songs.clone());
        artist.top_tracks = match top.is_empty() {
            true => songs.into_iter().take(TOP_TRACKS).collect(),
            false => top,
        };
        Ok(artist)
    }

    /// Only `getArtist`, so the page goes up as soon as the server names the artist and its
    /// albums. The top tracks follow in `artist_catalogue`.
    async fn artist_overview(&self, artist_id: &str) -> Result<Artist> {
        self.artist_detail(artist_id).await
    }

    /// The artist's most played songs, read after the overview is up. An artist nobody has
    /// played yet has none, and the page leaves the section out rather than listing songs in
    /// an arbitrary order. A failed search is an error, so the catalog asks again next time.
    async fn artist_catalogue(&self, artist_id: &str, _known: &[Track]) -> Result<ArtistCatalogue> {
        // Bounded like the search, so the page's popular skeleton always gives way: to the
        // songs, or to nothing once this has failed.
        let detail = tokio::time::timeout(LOCAL_WAIT, self.client.get_artist(artist_id))
            .await
            .with_context(|| format!("artist {artist_id} took too long"))?
            .context("cannot load the artist")?;
        let songs = self.artist_songs(artist_id, &detail.name).await?;
        Ok(ArtistCatalogue {
            top_tracks: most_played(songs),
            ..Default::default()
        })
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        let artist = self.artist_detail(artist_id).await?;
        Ok(ArtistProfile {
            name: artist.name,
            cover_large: artist.cover_large,
            biography: None,
        })
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        let mut tasks = JoinSet::new();
        for id in ids.into_iter().take(PORTRAIT_LIMIT) {
            let client = self.clone();
            tasks.spawn(async move {
                let detail = client.client.get_artist(&id).await.ok()?;
                let cover = client.artist_cover(detail.cover_art.as_deref(), &id)?;
                Some((id, cover))
            });
        }
        let mut images = HashMap::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(Some((id, image))) = result {
                images.insert(id, image);
            }
        }
        Ok(images)
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        let starred = self
            .client
            .get_starred2(None)
            .await
            .context("cannot load the starred songs")?;
        Ok(starred
            .song
            .into_iter()
            .map(|song| self.song(song))
            .collect())
    }

    /// Every song on the server. An empty `search3` query lists the whole library on
    /// OpenSubsonic servers, a page at a time.
    async fn all_tracks(&self) -> Result<Vec<Track>> {
        let mut tracks = Vec::new();
        let mut offset = 0i32;
        loop {
            let page = self
                .client
                .search3(
                    "",
                    Some(0),
                    None,
                    Some(0),
                    None,
                    Some(LIBRARY_PAGE),
                    Some(offset),
                    None,
                )
                .await
                .context("cannot load the songs")?;
            let fetched = page.song.len();
            tracks.extend(page.song.into_iter().map(|song| self.song(song)));
            if fetched < LIBRARY_PAGE as usize {
                break;
            }
            offset += fetched as i32;
        }
        Ok(tracks)
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        self.change_saved(saved, &[track_id], &[], &[])
            .await
            .with_context(|| format!("cannot change the star for {track_id}"))
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        let song = self
            .client
            .get_song(track_id)
            .await
            .with_context(|| format!("cannot load the song {track_id}"))?;
        Ok(self.song(song))
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        let song = self.client.get_song(track_id).await.ok();
        Ok(song
            .and_then(|song| song.play_count)
            .map(|count| count as u64))
    }

    /// Sends the position and state through `reportPlayback`, with scrobbling left to `played`.
    /// A server without the extension only hears that the track is playing, through the
    /// now-playing form of `scrobble`, since it has nowhere to put a position.
    async fn report(&self, track_id: &str, report: Report, position: Duration) -> Result<()> {
        if !self.reports_playback().await {
            return match report {
                Report::Playing => self
                    .client
                    .scrobble(track_id, None, Some(false))
                    .await
                    .with_context(|| format!("cannot report {track_id} as playing")),
                Report::Paused | Report::Stopped => Ok(()),
            };
        }
        let state = match report {
            Report::Playing => "playing",
            Report::Paused => "paused",
            Report::Stopped => "stopped",
        };
        let millis = i64::try_from(position.as_millis()).unwrap_or(i64::MAX);
        self.client
            .report_playback(track_id, "song", millis, state, None, Some(true))
            .await
            .with_context(|| format!("cannot report {track_id} as {state}"))
    }

    /// Subsonic takes the start of the listen in milliseconds since the epoch.
    async fn played(&self, track_id: &str, at: SystemTime) -> Result<()> {
        let millis = at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.client
            .scrobble(
                track_id,
                Some(i64::try_from(millis).unwrap_or(i64::MAX)),
                Some(true),
            )
            .await
            .with_context(|| format!("cannot record a play of {track_id}"))
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        Ok(self
            .client
            .get_playlists(None)
            .await
            .context("cannot load the playlists")?
            .iter()
            .map(|playlist| self.convert_playlist(playlist))
            .collect())
    }

    async fn create_playlist(&self, name: &str) -> Result<String> {
        let created = self
            .client
            .create_playlist(None, Some(name), &[])
            .await
            .context("cannot create the playlist")?;
        Ok(created.id)
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        self.client
            .update_playlist(playlist_id, Some(name), None, None, &[], &[])
            .await
            .context("cannot rename the playlist")
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.client
            .delete_playlist(playlist_id)
            .await
            .context("cannot delete the playlist")
    }

    async fn remove_playlist_from_library(&self, _playlist_id: &str) -> Result<()> {
        Ok(())
    }

    async fn add_playlist_to_library(&self, _playlist_id: &str) -> Result<()> {
        Ok(())
    }

    async fn set_playlist_public(&self, playlist_id: &str, public: bool) -> Result<()> {
        self.client
            .update_playlist(playlist_id, None, None, Some(public), &[], &[])
            .await
            .context("cannot change the playlist visibility")
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.client
            .update_playlist(playlist_id, None, None, None, &[track_id], &[])
            .await
            .context("cannot add the track to the playlist")
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        let detail = self
            .client
            .get_playlist(playlist_id)
            .await
            .context("cannot load the playlist")?;
        let index = detail
            .entry
            .iter()
            .position(|song| song.id == track_id)
            .context("track is not in the playlist")? as i32;
        self.client
            .update_playlist(playlist_id, None, None, None, &[], &[index])
            .await
            .context("cannot remove the track from the playlist")
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        let starred = self
            .client
            .get_starred2(None)
            .await
            .context("cannot load the starred albums")?;
        Ok(starred
            .album
            .into_iter()
            .map(|album| self.convert_album(album))
            .collect())
    }

    async fn all_albums(&self) -> Result<Vec<Album>> {
        let mut albums = Vec::new();
        let mut offset = 0i32;
        loop {
            let page = self
                .client
                .get_album_list2(
                    AlbumListType::AlphabeticalByName,
                    Some(LIBRARY_PAGE),
                    Some(offset),
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .context("cannot load the albums")?;
            let fetched = page.len();
            albums.extend(page.into_iter().map(|album| self.convert_album(album)));
            if fetched == 0 {
                break;
            }
            offset += fetched as i32;
        }
        Ok(albums)
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        self.change_saved(saved, &[], &[album_id], &[])
            .await
            .with_context(|| format!("cannot change the star for album {album_id}"))
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        let starred = self
            .client
            .get_starred2(None)
            .await
            .context("cannot load the starred artists")?;
        Ok(starred
            .artist
            .iter()
            .map(|artist| {
                let cover = self.artist_cover(artist.cover_art.as_deref(), &artist.id);
                wire::saved_artist(artist, cover)
            })
            .collect())
    }

    async fn all_artists(&self) -> Result<Vec<SavedArtist>> {
        let artists = self
            .client
            .get_artists(None)
            .await
            .context("cannot load the artists")?;
        Ok(artists
            .index
            .into_iter()
            .flat_map(|index| index.artist)
            .map(|artist| {
                let cover = self.artist_cover(artist.cover_art.as_deref(), &artist.id);
                wire::saved_artist(&artist, cover)
            })
            .collect())
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        self.change_saved(saved, &[], &[], &[artist_id])
            .await
            .with_context(|| format!("cannot change the star for artist {artist_id}"))
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        let detail = self
            .client
            .get_album(album_id)
            .await
            .with_context(|| format!("cannot load the album {album_id}"))?;
        let album = self.detail_album(&detail, detail.song.len());
        let mut tracks: Vec<Track> = detail
            .song
            .into_iter()
            .map(|song| self.song(song))
            .collect();
        // a server answers in whatever order it keeps; an album is heard in disc and track order
        tracks.sort_by(|a, b| {
            (a.disc_number, a.track_number)
                .cmp(&(b.disc_number, b.track_number))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(AlbumDetail { album, tracks })
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        Ok(self.album(album_id).await?.tracks)
    }

    async fn album_catalogue(
        &self,
        album_id: &str,
        artist_id: Option<&str>,
    ) -> Result<AlbumCatalogue> {
        let Some(artist_id) = artist_id else {
            return Ok(AlbumCatalogue::default());
        };
        // Only the artist's own albums: the similar artists `getArtistInfo2` lists come from
        // Last.fm, and nothing on this path leaves the server. Nothing read is an error rather
        // than an empty rail, so the catalog does not keep it for the rest of the session.
        let mut seen = HashSet::new();
        let also_like = self
            .more_from_artist(album_id, artist_id)
            .await
            .context("cannot read any recommendations")?
            .into_iter()
            .filter(|album| seen.insert(album.id.clone()))
            .collect();
        Ok(AlbumCatalogue {
            also_like,
            similar: Vec::new(),
        })
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let detail = self
            .client
            .get_playlist(playlist_id)
            .await
            .with_context(|| format!("cannot load the playlist {playlist_id}"))?;
        let tracks: Vec<Track> = detail
            .entry
            .iter()
            .map(|song| self.song(song.clone()))
            .collect();
        let cover = detail
            .cover_art
            .as_deref()
            .and_then(|id| self.cover_url(id, 300));
        let mut playlist = wire::playlist(
            &detail.id,
            &detail.name,
            detail.owner.as_deref(),
            detail.public.unwrap_or(false),
            detail.song_count.unwrap_or(0).max(0) as u32,
            cover,
            &self.username,
        );
        if playlist.track_count == 0 {
            playlist.track_count = tracks.len() as u32;
        }
        if playlist.cover.is_none() {
            playlist.cover = tracks.iter().find_map(|track| track.cover.clone());
        }
        Ok(PlaylistDetail {
            playlist,
            tracks,
            continuation: None,
        })
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        Ok(self.playlist(playlist_id).await?.tracks)
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        let tracks = self.playlist_tracks(playlist_id).await?;
        Ok(distinct_covers(&tracks, wanted))
    }

    async fn track_radio(
        &self,
        track_id: &str,
        _from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        // Random songs from the library rather than `getSimilarSongs2`, which Navidrome
        // answers by asking Last.fm for similar artists.
        let random = self
            .client
            .get_random_songs(Some(RADIO_COUNT), None, None, None, None)
            .await
            .context("cannot load a radio")?;
        Ok((
            random
                .into_iter()
                .map(|song| self.song(song))
                .filter(|track| track.id.as_deref() != Some(track_id))
                .collect(),
            None,
        ))
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let found = self
            .client
            .search3(query, None, None, None, None, Some(50), None, None)
            .await
            .context("cannot search")?;
        Ok(found.song.into_iter().map(|song| self.song(song)).collect())
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        let found = self
            .client
            .search3(query, None, None, Some(30), None, None, None, None)
            .await
            .context("cannot search albums")?;
        Ok(found
            .album
            .into_iter()
            .map(|album| self.convert_album(album))
            .collect())
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        let needle = query.to_lowercase();
        Ok(self
            .playlists()
            .await?
            .into_iter()
            .filter(|playlist| playlist.name.to_lowercase().contains(&needle))
            .collect())
    }

    async fn home(&self) -> Result<HomeFeed> {
        let random = self
            .client
            .get_random_songs(Some(HOME_SONGS), None, None, None, None)
            .await
            .map(|songs| {
                songs
                    .into_iter()
                    .map(|song| self.song(song))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let newest = self.album_list(AlbumListType::Newest, HOME_ALBUMS).await;
        let frequent = self.album_list(AlbumListType::Frequent, HOME_ALBUMS).await;

        let mut sections = Vec::new();
        if !newest.is_empty() {
            sections.push(GenreSection {
                title: "Newest albums".to_owned(),
                items: newest.into_iter().map(GenreItem::Album).collect(),
            });
        }
        if !frequent.is_empty() {
            sections.push(GenreSection {
                title: "Most played albums".to_owned(),
                items: frequent.into_iter().map(GenreItem::Album).collect(),
            });
        }

        Ok(HomeFeed {
            listen_again: random
                .iter()
                .take(10)
                .cloned()
                .map(GenreItem::Track)
                .collect(),
            quick_picks: Some(random.into_iter().take(15).collect()),
            sections,
        })
    }

    async fn genres(&self) -> Result<Vec<Genre>> {
        let genres = self
            .client
            .get_genres()
            .await
            .context("cannot load the genres")?;
        Ok(genres.into_iter().map(source_genre).collect())
    }

    async fn genre(&self, genre_id: &str) -> Result<GenreDetail> {
        let albums = self
            .client
            .get_album_list2(
                AlbumListType::ByGenre,
                Some(50),
                None,
                None,
                None,
                Some(genre_id),
                None,
            )
            .await
            .with_context(|| format!("cannot load the genre {genre_id}"))?;
        let items: Vec<GenreItem> = albums
            .into_iter()
            .map(|album| GenreItem::Album(self.convert_album(album)))
            .collect();
        Ok(GenreDetail {
            name: genre_id.to_owned(),
            sections: match items.is_empty() {
                true => Vec::new(),
                false => vec![GenreSection {
                    title: genre_id.to_owned(),
                    items,
                }],
            },
        })
    }
}

impl SubsonicClient {
    async fn change_saved(
        &self,
        saved: bool,
        ids: &[&str],
        album_ids: &[&str],
        artist_ids: &[&str],
    ) -> Result<()> {
        match saved {
            true => self.client.star(ids, album_ids, artist_ids).await?,
            false => self.client.unstar(ids, album_ids, artist_ids).await?,
        }
        Ok(())
    }

    async fn album_list(
        &self,
        kind: opensubsonic::api::lists::AlbumListType,
        size: i32,
    ) -> Vec<Album> {
        self.client
            .get_album_list2(kind, Some(size), None, None, None, None, None)
            .await
            .map(|albums| {
                albums
                    .into_iter()
                    .map(|album| self.convert_album(album))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Structured lyrics Navidrome (and other OpenSubsonic servers) keep for a song: embedded
    /// tags, sidecar `.lrc` files the server scanned, and anything else `getLyricsBySongId`
    /// returns. `enhanced` asks for word-level cues when the server supports songLyrics v2.
    pub async fn lyrics(
        &self,
        track_id: &str,
        enhanced: bool,
    ) -> Result<opensubsonic::data::LyricsList> {
        self.client
            .get_lyrics_by_song_id(track_id, Some(enhanced).filter(|value| *value))
            .await
            .with_context(|| format!("cannot load lyrics for {track_id}"))
    }

    /// Opens the original audio download and answers once the response headers are in; the
    /// body is still on its way. Used to stream into the offline cache without holding the
    /// whole file in RAM.
    /// Fetches a small server resource (such as a cover image) whose url this client built.
    pub async fn fetch_bytes(&self, url: &str) -> Result<bytes::Bytes> {
        self.http
            .get(url)
            .send()
            .await
            .with_context(|| format!("cannot fetch {url}"))?
            .error_for_status()
            .context("the server refused the request")?
            .bytes()
            .await
            .context("the response body broke")
    }

    pub async fn open_download(&self, track_id: &str) -> Result<reqwest::Response> {
        let url = format!("{}&id={}", self.downloads, escape::component(track_id));
        self.http
            .get(url)
            .send()
            .await
            .with_context(|| format!("cannot download {track_id}"))?
            .error_for_status()
            .with_context(|| format!("the server refused the download for {track_id}"))
    }

    /// Opens the audio of a track and answers once the response headers are in; the body is
    /// still on its way.
    pub async fn open_stream(&self, track_id: &str) -> Result<reqwest::Response> {
        let url = self
            .client
            .stream_url(track_id, None, None)
            .context("cannot build the stream url")?;
        self.http
            .get(url)
            .send()
            .await
            .context("cannot stream the track")?
            .error_for_status()
            .context("the server refused the stream")
    }

    /// The length and ReplayGain the server records for a track, whichever it has. The track
    /// gain wins, and the album gain stands in when that is all the server knows.
    pub async fn details(&self, track_id: &str) -> Details {
        let Ok(song) = self.client.get_song(track_id).await else {
            return Details::default();
        };
        let duration = song
            .duration
            .map(|seconds| Duration::from_secs(u64::try_from(seconds).unwrap_or(0)));
        let loudness = song.replay_gain.and_then(|gain| {
            let (gain, peak) = match gain.track_gain {
                Some(track) => (track, gain.track_peak),
                None => (gain.album_gain?, gain.album_peak),
            };
            Some(Loudness::replay_gain(
                gain as f32,
                peak.map(|peak| peak as f32),
            ))
        });
        Details { duration, loudness }
    }
}

fn source_genre(source: SourceGenre) -> Genre {
    let name = source.name;
    Genre {
        id: name.clone(),
        name,
        cover: None,
    }
}

/// The songs credited to the artist, by id where the server gives one and by name where it
/// does not. A name search also finds songs that only mention the name in a title or album.
fn credited(artist_id: &str, name: &str, songs: Vec<Track>) -> Vec<Track> {
    songs
        .into_iter()
        .filter(|track| {
            track.artist_refs.iter().any(|artist| match &artist.id {
                Some(id) if !id.is_empty() => id == artist_id,
                _ => artist.name.eq_ignore_ascii_case(name),
            })
        })
        .collect()
}

/// The `TOP_TRACKS` most played songs, most played first. Songs never played are left out,
/// so an artist nobody has listened to has no top tracks rather than an arbitrary list.
fn most_played(songs: Vec<Track>) -> Vec<Track> {
    let mut played: Vec<Track> = songs
        .into_iter()
        .filter(|track| track.playcount.unwrap_or(0) > 0)
        .collect();
    // stable, so songs played as often keep the order the server gave them
    played.sort_by(|a, b| b.playcount.cmp(&a.playcount));
    played.truncate(TOP_TRACKS);
    played
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ArtistRef;

    fn song(id: &str, artist: (&str, Option<&str>), plays: Option<u64>) -> Track {
        Track {
            id: Some(id.to_owned()),
            name: id.to_owned(),
            playable: true,
            artists: artist.0.to_owned(),
            artist_refs: vec![ArtistRef {
                name: artist.0.to_owned(),
                id: artist.1.map(str::to_owned),
            }],
            album: String::new(),
            album_id: None,
            cover: None,
            duration: Duration::ZERO,
            added_at: None,
            added_by: None,
            playcount: plays,
            popularity: 0,
            explicit: false,
            track_number: 0,
            disc_number: 0,
            tags: Vec::new(),
            languages: Vec::new(),
            credits: Vec::new(),
        }
    }

    fn ids(tracks: &[Track]) -> Vec<&str> {
        tracks
            .iter()
            .filter_map(|track| track.id.as_deref())
            .collect()
    }

    #[test]
    fn keeps_only_songs_credited_to_the_artist() {
        let songs = vec![
            song("own", ("Muse", Some("ar-1")), None),
            song("other", ("Muse Tribute", Some("ar-2")), None),
            song("named", ("muse", None), None),
            song("stranger", ("Someone", None), None),
        ];
        assert_eq!(ids(&credited("ar-1", "Muse", songs)), ["own", "named"]);
    }

    #[test]
    fn ranks_played_songs_and_drops_the_unplayed() {
        let songs = vec![
            song("never", ("A", Some("a")), None),
            song("once", ("A", Some("a")), Some(1)),
            song("zero", ("A", Some("a")), Some(0)),
            song("often", ("A", Some("a")), Some(9)),
        ];
        assert_eq!(ids(&most_played(songs)), ["often", "once"]);
    }

    #[test]
    fn an_unplayed_artist_has_no_top_tracks() {
        let songs = vec![
            song("a", ("A", Some("a")), Some(0)),
            song("b", ("A", Some("a")), None),
        ];
        assert!(most_played(songs).is_empty());
    }

    #[test]
    fn caps_the_top_tracks() {
        let songs = (0..TOP_TRACKS as u64 + 5)
            .map(|plays| song(&plays.to_string(), ("A", Some("a")), Some(plays + 1)))
            .collect();
        let top = most_played(songs);
        assert_eq!(top.len(), TOP_TRACKS);
        assert_eq!(top[0].playcount, Some(TOP_TRACKS as u64 + 5));
    }
}
