use std::path::PathBuf;

use anyhow::{Context as _, Result};
use opensubsonic::Auth;
use serde::{Deserialize, Serialize};

use crate::credentials;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    /// Currently active base URL used for API calls.
    pub server: String,
    /// Every address for this account (LAN, public, …). The first entry is the stable
    /// "home" URL used to key offline caches across switches. Empty on older files —
    /// [`Credentials::addresses`] fills it from `server`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Prefer alternate / remote addresses when probing (Feishin `preferRemoteUrl`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prefer_remote: bool,
    pub username: String,
    pub password: String,
    /// The token and salt every cover url is signed with. Made once at sign-in and kept, so a
    /// cover keeps one url across launches and the image caches can hold it.
    #[serde(default)]
    pub signature: Option<Signature>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub token: String,
    pub salt: String,
}

/// Signs once with a fresh salt, the way the server expects each request to be signed.
pub fn sign(username: &str, password: &str) -> Signature {
    let mut signature = Signature {
        token: String::new(),
        salt: String::new(),
    };
    for (key, value) in Auth::token(username, password).params() {
        match key {
            "t" => signature.token = value,
            "s" => signature.salt = value,
            _ => {}
        }
    }
    signature
}

fn path() -> PathBuf {
    credentials::dir("subsonic").join(credentials::FILE)
}

pub fn normalize_server(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        anyhow::bail!("the server address is empty");
    }
    let with_scheme = match trimmed.contains("://") {
        true => trimmed.to_owned(),
        false => format!("http://{trimmed}"),
    };
    Ok(with_scheme)
}

/// Split a free-text field into distinct normalized base URLs (lines, commas, or spaces).
pub fn parse_urls(raw: &str) -> Result<Vec<String>> {
    let mut urls = Vec::new();
    for piece in raw.split(['\n', '\r', ',', ';']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let url = normalize_server(piece)?;
        if !urls.iter().any(|known| known == &url) {
            urls.push(url);
        }
    }
    if urls.is_empty() {
        anyhow::bail!("the server address is empty");
    }
    Ok(urls)
}

impl Credentials {
    /// Distinct addresses for this account, preferred first when probing.
    pub fn addresses(&self) -> Vec<String> {
        let mut urls = if self.urls.is_empty() {
            vec![self.server.clone()]
        } else {
            self.urls.clone()
        };
        if !urls.iter().any(|url| url == &self.server) && !self.server.is_empty() {
            urls.insert(0, self.server.clone());
        }
        if self.prefer_remote && urls.len() > 1 {
            let primary = urls.remove(0);
            urls.push(primary);
        }
        urls
    }

    /// Stable key for offline caches: the home (first configured) URL, not the active one.
    pub fn account_key(&self) -> String {
        self.urls
            .first()
            .cloned()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| self.server.clone())
    }

    pub fn normalize(&mut self) -> Result<()> {
        self.server = normalize_server(&self.server)?;
        let mut urls = Vec::new();
        for url in std::mem::take(&mut self.urls) {
            let url = normalize_server(&url)?;
            if !urls.iter().any(|known| known == &url) {
                urls.push(url);
            }
        }
        if urls.is_empty() {
            urls.push(self.server.clone());
        } else if !urls.iter().any(|url| url == &self.server) {
            urls.insert(0, self.server.clone());
        }
        self.urls = urls;
        Ok(())
    }
}

pub fn load() -> Option<Credentials> {
    let bytes = std::fs::read(path()).ok()?;
    let mut credentials: Credentials = serde_json::from_slice(&bytes).ok()?;
    credentials.normalize().ok()?;
    match credentials.username.is_empty() {
        true => None,
        false => Some(credentials),
    }
}

pub fn store(stored: &Credentials) -> Result<()> {
    let mut stored = stored.clone();
    stored.normalize()?;
    let bytes =
        serde_json::to_vec_pretty(&stored).context("cannot serialize subsonic credentials")?;
    credentials::write(&path(), &bytes).context("cannot store subsonic credentials")
}

pub fn forget() {
    credentials::remove(&path());
    crate::subsonic::connection::invalidate(None);
}
