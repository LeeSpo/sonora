use std::time::Duration;

use gpui::{App, Context, Entity, EventEmitter, Task};

use crate::session::Session;
use crate::{Io, Sonora};

/// How long to wait before each check of whether the network is back. The last one repeats
/// for as long as the network stays gone.
const PROBE: [Duration; 4] = [
    Duration::from_secs(3),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// How long a check may take before it counts as a failure.
const PATIENCE: Duration = Duration::from_secs(4);

/// Raised when the network came back, so a screen that gave up can load again.
pub struct Reconnected;

/// Whether the app can reach the network at all, as one answer for the whole app rather than
/// one per screen. It goes offline on the first failure that reads as a lost connection and
/// comes back either when something reaches the network or when its own check does.
pub struct Network {
    session: Entity<Session>,
    io: Io,
    lost: bool,
    probe: Option<Task<()>>,
}

impl EventEmitter<Reconnected> for Network {}

impl Network {
    pub fn new(session: Entity<Session>, io: Io) -> Self {
        Self {
            session,
            io,
            lost: false,
            probe: None,
        }
    }

    pub fn global(cx: &App) -> Entity<Self> {
        Sonora::global(cx).network.clone()
    }

    /// Whether the network is gone. Every screen that needs one asks this rather than waiting
    /// for its own load to fail.
    pub fn lost(cx: &App) -> bool {
        match cx.try_global::<Sonora>() {
            Some(sonora) => sonora.network.read(cx).lost,
            None => false,
        }
    }

    /// Notes a failed call. Only a reason that reads as a lost connection puts the app offline;
    /// anything the provider refused leaves it alone.
    pub fn failed(reason: &str, cx: &mut App) {
        if !music::trouble::offline(reason) {
            return;
        }
        let Some(network) = cx
            .try_global::<Sonora>()
            .map(|sonora| sonora.network.clone())
        else {
            return;
        };
        network.update(cx, |this, cx| {
            if this.lost {
                return;
            }
            log::warn!("network: the connection is gone: {reason}");
            this.lost = true;
            this.watch(cx);
            cx.notify();
            cx.refresh_windows();
        });
    }

    /// Notes that something reached the network, which is the quickest way back online.
    pub fn reached(cx: &mut App) {
        let Some(network) = cx
            .try_global::<Sonora>()
            .map(|sonora| sonora.network.clone())
        else {
            return;
        };
        network.update(cx, |this, cx| this.found(cx));
    }

    fn found(&mut self, cx: &mut Context<Self>) {
        if !self.lost {
            return;
        }
        log::info!("network: the connection is back");
        self.lost = false;
        self.probe = None;
        cx.notify();
        cx.refresh_windows();
        cx.emit(Reconnected);
    }

    /// Opens a connection to the active provider's host until one succeeds, waiting longer
    /// between tries as they keep failing. Nothing is asked of the host but the connection, and
    /// it is one the app talks to anyway, so no other service ever learns Sonora is running. A
    /// provider that needs no network hands back no host, and then nothing is tried at all.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let io = self.io.clone();

        self.probe = Some(cx.spawn(async move |this, cx| {
            for tried in 0.. {
                let wait = PROBE[tried.min(PROBE.len() - 1)];
                cx.background_executor().timer(wait).await;

                let Ok(targets) = this.update(cx, |this, cx| this.session.read(cx).reach()) else {
                    return;
                };
                if targets.is_empty() {
                    continue;
                }
                // Each configured address on its own port (e.g. a Subsonic server on :9999);
                // probing a fixed 443 could "succeed" against an unrelated listener and send the
                // session into a reconnect loop while the provider itself is still unreachable.
                let reached = io
                    .spawn(async move {
                        for (host, port) in targets {
                            let connect = tokio::net::TcpStream::connect((host.as_str(), port));
                            if let Ok(Ok(_)) = tokio::time::timeout(PATIENCE, connect).await {
                                return true;
                            }
                        }
                        false
                    })
                    .await;
                if matches!(reached, Ok(true)) {
                    this.update(cx, |this, cx| this.found(cx)).ok();
                    return;
                }
            }
        }));
    }
}
