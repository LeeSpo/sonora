pub(crate) mod auth;
mod client;
pub(crate) mod connection;
mod lyrics;
pub mod offline;
mod playback;
mod wire;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub use client::SubsonicClient;
pub use connection::{active as active_url, select as select_url, urls as server_urls};
pub use lyrics::SubsonicLyrics;

use crate::subsonic::playback::Factory;
use crate::{Capabilities, MusicApi as _, MusicProvider, ProviderSession, Shape, SignIn};

pub struct SubsonicProvider;

impl SubsonicProvider {
    pub fn new() -> Self {
        Self
    }

    async fn connect(
        server: String,
        remotes: Vec<String>,
        prefer_remote: bool,
        username: String,
        password: String,
    ) -> Result<ProviderSession> {
        let mut urls = auth::parse_urls(&server)?;
        for remote in remotes {
            for url in auth::parse_urls(&remote).unwrap_or_default() {
                if !urls.iter().any(|known| known == &url) {
                    urls.push(url);
                }
            }
        }
        let home = urls[0].clone();
        let mut draft = auth::Credentials {
            server: home.clone(),
            urls: urls.clone(),
            prefer_remote,
            username: username.clone(),
            password: password.clone(),
            signature: None,
        };
        draft.normalize()?;

        let selected = connection::resolve(&draft).await;
        let signature = auth::sign(&username, &password);
        let client = SubsonicClient::new(
            selected.clone(),
            username.clone(),
            password.clone(),
            &signature,
        )?;
        let profile = client
            .profile()
            .await
            .context("cannot reach the subsonic server")?;
        auth::store(&auth::Credentials {
            server: selected,
            urls,
            prefer_remote,
            username,
            password,
            signature: Some(signature),
        })?;
        Ok(ProviderSession {
            profile,
            api: Arc::new(client.clone()),
            playback: Arc::new(Factory::new(client)),
            shape: Shape::Catalog,
            authenticated: true,
            capabilities: Capabilities::ALL,
        })
    }

    async fn restore_stored() -> Result<Option<ProviderSession>> {
        let Some(mut remembered) = auth::load() else {
            return Ok(None);
        };
        if remembered.signature.is_none() {
            remembered.signature = Some(auth::sign(&remembered.username, &remembered.password));
            if let Err(error) = auth::store(&remembered) {
                log::warn!("subsonic: cannot keep the cover signature: {error:#}");
            }
        }

        let selected = connection::resolve(&remembered).await;
        if selected != remembered.server {
            log::info!("subsonic: switching server address to {selected}");
            remembered.server = selected.clone();
            if let Err(error) = auth::store(&remembered) {
                log::warn!("subsonic: cannot keep the active server address: {error:#}");
            }
        }

        let signature = remembered.signature.clone().unwrap_or_default();
        let client = SubsonicClient::new(
            selected,
            remembered.username.clone(),
            remembered.password.clone(),
            &signature,
        )?;
        match client.profile().await {
            Ok(profile) => Ok(Some(ProviderSession {
                profile,
                api: Arc::new(client.clone()),
                playback: Arc::new(Factory::new(client)),
                shape: Shape::Catalog,
                authenticated: true,
                capabilities: Capabilities::ALL,
            })),
            Err(error) if crate::trouble::offline(&format!("{error:#}")) => {
                // Current pick failed: invalidate and try each remaining address once.
                connection::invalidate(Some(&remembered.server));
                for address in remembered.addresses() {
                    if address == remembered.server {
                        continue;
                    }
                    let Ok(fallback) = SubsonicClient::new(
                        address.clone(),
                        remembered.username.clone(),
                        remembered.password.clone(),
                        &signature,
                    ) else {
                        continue;
                    };
                    if let Ok(profile) = fallback.profile().await {
                        log::info!("subsonic: failing over to {address}");
                        remembered.server = address;
                        if let Err(store_error) = auth::store(&remembered) {
                            log::warn!(
                                "subsonic: cannot keep the failover address: {store_error:#}"
                            );
                        }
                        return Ok(Some(ProviderSession {
                            profile,
                            api: Arc::new(fallback.clone()),
                            playback: Arc::new(Factory::new(fallback)),
                            shape: Shape::Catalog,
                            authenticated: true,
                            capabilities: Capabilities::ALL,
                        }));
                    }
                }
                Err(error)
            }
            Err(error) => {
                log::warn!("subsonic: the stored session is no longer usable: {error:#}");
                Ok(None)
            }
        }
    }
}

impl Default for SubsonicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MusicProvider for SubsonicProvider {
    fn name(&self) -> &'static str {
        "Subsonic"
    }

    fn slug(&self) -> &'static str {
        "subsonic"
    }

    fn sign_in_options(&self) -> Vec<SignIn> {
        vec![SignIn::Credentials {
            server: String::new(),
            remotes: Vec::new(),
            prefer_remote: false,
            username: String::new(),
            password: String::new(),
        }]
    }

    fn stored(&self) -> bool {
        auth::load().is_some()
    }

    fn location(&self) -> Option<String> {
        auth::load().map(|credentials| credentials.server)
    }

    fn locations(&self) -> Vec<String> {
        auth::load()
            .map(|credentials| {
                let mut urls = credentials.urls;
                if urls.is_empty() {
                    urls.push(credentials.server);
                }
                urls
            })
            .unwrap_or_default()
    }

    fn select_location(&self, location: &str) -> Result<bool> {
        let before = auth::load().map(|credentials| credentials.server);
        connection::select(location)?;
        let after = auth::load().map(|credentials| credentials.server);
        Ok(before != after)
    }

    /// The configured server's host, since a self-hosted library has no address in common with
    /// anyone else's.
    fn reach(&self) -> Option<String> {
        let server = auth::load()?.server;
        let host = server
            .split_once("://")
            .map_or(server.as_str(), |(_, rest)| rest);
        let host = host.split(['/', ':', '?']).next()?;
        (!host.is_empty()).then(|| host.to_owned())
    }

    async fn restore(&self) -> Result<Option<ProviderSession>> {
        Self::restore_stored().await
    }

    async fn sign_in(
        &self,
        method: SignIn,
        _prompt: crate::PromptSink,
        _input: crate::InputSource,
    ) -> Result<ProviderSession> {
        let SignIn::Credentials {
            server,
            remotes,
            prefer_remote,
            username,
            password,
        } = method
        else {
            anyhow::bail!("subsonic signs in with a server address, username and password")
        };
        if username.trim().is_empty() {
            anyhow::bail!("the subsonic username is empty");
        }
        if password.is_empty() {
            anyhow::bail!("the subsonic password is empty");
        }
        Self::connect(server, remotes, prefer_remote, username, password).await
    }

    fn sign_out(&self) {
        auth::forget();
    }
}
