//! Feishin-like multi-address switching for one Subsonic / Navidrome account.
//!
//! An account may list several base URLs (LAN and public, Tailscale and local, …).
//! Probes prefer the configured order, keep the reachable one, and switch when the
//! current address fails or the user picks another.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use opensubsonic::{Auth, Client, Error as SubsonicError};

use crate::subsonic::auth::{self, Credentials};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const CHECK_INTERVAL: Duration = Duration::from_secs(30);
const CLIENT_NAME: &str = "sonora";

#[derive(Default)]
struct Cache {
    key: String,
    selected: String,
    checked_at: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    key: String::new(),
    selected: String::new(),
    checked_at: None,
});

fn cache_key(credentials: &Credentials) -> String {
    format!(
        "{}|{}|{}",
        credentials.prefer_remote,
        credentials.username,
        credentials.addresses().join("\n")
    )
}

/// Whether `base` answers a Subsonic ping quickly enough to count as reachable.
/// An API error (wrong password, …) still means the host is up — same idea as Feishin.
pub async fn probe(base: &str, username: &str, password: &str) -> bool {
    let Ok(client) = Client::new(base, Auth::token(username, password)) else {
        return false;
    };
    let client = client.with_client_name(CLIENT_NAME);
    match tokio::time::timeout(PROBE_TIMEOUT, client.ping()).await {
        Ok(Ok(())) => true,
        Ok(Err(SubsonicError::Api(_))) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

/// Pick a reachable address in preference order. Falls back to the current `server`
/// when nothing answers, so callers can still try the last known URL.
pub async fn resolve(credentials: &Credentials) -> String {
    let addresses = credentials.addresses();
    if addresses.is_empty() {
        return credentials.server.clone();
    }
    if addresses.len() == 1 {
        return addresses[0].clone();
    }

    let key = cache_key(credentials);
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.key == key
            && let Some(checked) = cache.checked_at
            && checked.elapsed() < CHECK_INTERVAL
            && !cache.selected.is_empty()
        {
            return cache.selected.clone();
        }
    }

    let mut selected = credentials.server.clone();
    for address in &addresses {
        if probe(address, &credentials.username, &credentials.password).await {
            selected = address.clone();
            break;
        }
    }

    if let Ok(mut cache) = CACHE.lock() {
        cache.key = key;
        cache.selected = selected.clone();
        cache.checked_at = Some(Instant::now());
    }
    selected
}

/// Forget the cached pick so the next resolve probes again (e.g. after a failure).
pub fn invalidate(failed: Option<&str>) {
    let Ok(mut cache) = CACHE.lock() else {
        return;
    };
    if let Some(failed) = failed
        && !cache.selected.is_empty()
        && cache.selected != failed
    {
        return;
    }
    cache.checked_at = None;
}

/// Persist `url` as the active address when it belongs to the account, and invalidate
/// the probe cache so the next resolve keeps the user's pick.
pub fn select(url: &str) -> Result<()> {
    let mut credentials = auth::load().context("no subsonic account is stored")?;
    let url = auth::normalize_server(url)?;
    if !credentials.addresses().iter().any(|known| known == &url) {
        bail!("that address is not configured for this account");
    }
    credentials.server = url.clone();
    auth::store(&credentials)?;
    invalidate(None);
    if let Ok(mut cache) = CACHE.lock() {
        cache.key = cache_key(&credentials);
        cache.selected = url;
        cache.checked_at = Some(Instant::now());
    }
    Ok(())
}

/// Every configured address for the stored account.
pub fn urls() -> Vec<String> {
    auth::load()
        .map(|credentials| credentials.addresses())
        .unwrap_or_default()
}

/// The address currently selected for the stored account.
pub fn active() -> Option<String> {
    auth::load().map(|credentials| credentials.server)
}
