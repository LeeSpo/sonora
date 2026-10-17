use std::time::{Duration, Instant};

use gpui::{App, Context, Entity, EventEmitter, Task};

use crate::session::Session;
use crate::{Io, Sonora};

/// How long a plain connection check may take before it counts as a failure.
const PATIENCE: Duration = Duration::from_secs(4);
/// The least time a manual reconnect shows as busy, so a check that fails at once still gives
/// the click visible feedback before "still offline" appears.
const FEEDBACK: Duration = Duration::from_secs(1);

/// Raised when the network came back, so a screen that gave up can load again.
pub struct Reconnected;

/// Whether the app can reach the network at all, as one answer for the whole app rather than
/// one per screen.
///
/// It goes offline on the first failure that reads as a lost connection and then stays there:
/// nothing checks in the background and no other call can bring it back. Only the user asking
/// to reconnect does, after the provider itself answers. Any automatic path back (a background
/// probe, a third-party lyrics answer, a load from cache) used to flip every screen between a
/// reload and the No connection notice over and over while the server stayed unreachable.
pub struct Network {
    session: Entity<Session>,
    io: Io,
    lost: bool,
    /// A manual check is running; only its button shows that.
    checking: bool,
    /// The last manual check found the provider still unreachable.
    still_offline: bool,
    check: Option<Task<()>>,
}

impl EventEmitter<Reconnected> for Network {}

impl Network {
    pub fn new(session: Entity<Session>, io: Io) -> Self {
        Self {
            session,
            io,
            lost: false,
            checking: false,
            still_offline: false,
            check: None,
        }
    }

    pub fn global(cx: &App) -> Entity<Self> {
        Sonora::global(cx).network.clone()
    }

    fn read(cx: &App) -> Option<&Self> {
        cx.try_global::<Sonora>()
            .map(|sonora| sonora.network.read(cx))
    }

    /// Whether the network is gone. Every screen that needs one asks this rather than waiting
    /// for its own load to fail.
    pub fn lost(cx: &App) -> bool {
        Self::read(cx).is_some_and(|this| this.lost)
    }

    /// Whether a manual reconnect is being checked right now.
    pub fn checking(cx: &App) -> bool {
        Self::read(cx).is_some_and(|this| this.checking)
    }

    /// Whether the last manual reconnect found the provider still unreachable.
    pub fn still_offline(cx: &App) -> bool {
        Self::read(cx).is_some_and(|this| this.lost && this.still_offline)
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
            this.still_offline = false;
            cx.notify();
            cx.refresh_windows();
        });
    }

    /// Notes that a provider call got through. While offline this changes nothing: coming back
    /// is the user's call (see [`Network::reconnect`]), so a stray success cannot flip screens.
    pub fn reached(cx: &mut App) {
        if Self::lost(cx) {
            log::debug!("network: a call got through while offline; waiting for a reconnect");
        }
    }

    /// Asks the active provider once whether it answers (a real Subsonic ping where it can),
    /// and on success brings the app back online. Nothing on the page changes while it runs;
    /// a failure only marks [`Network::still_offline`]. Playback is left alone either way.
    pub fn reconnect(cx: &mut App) {
        let Some(network) = cx
            .try_global::<Sonora>()
            .map(|sonora| sonora.network.clone())
        else {
            return;
        };
        network.update(cx, |this, cx| this.check(cx));
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        if !self.lost || self.checking {
            return;
        }
        self.checking = true;
        self.still_offline = false;
        cx.notify();
        cx.refresh_windows();

        let session = self.session.read(cx);
        let provider = session.active_provider();
        let targets = session.reach();
        let io = self.io.clone();
        let started = Instant::now();
        self.check = Some(cx.spawn(async move |this, cx| {
            let reached = io
                .spawn(async move {
                    if let Some(provider) = provider
                        && let Some(answer) = provider.reachable().await
                    {
                        return answer;
                    }
                    for (host, port) in targets {
                        let connect = tokio::net::TcpStream::connect((host.as_str(), port));
                        if let Ok(Ok(_)) = tokio::time::timeout(PATIENCE, connect).await {
                            return true;
                        }
                    }
                    false
                })
                .await;
            let reached = matches!(reached, Ok(true));
            let wait = feedback_left(started.elapsed());
            if !wait.is_zero() {
                cx.background_executor().timer(wait).await;
            }
            this.update(cx, |this, cx| {
                this.checking = false;
                this.check = None;
                match reached {
                    true => this.found(cx),
                    false => {
                        log::info!("network: still unreachable after a manual reconnect");
                        this.still_offline = true;
                        cx.notify();
                        cx.refresh_windows();
                    }
                }
            })
            .ok();
        }));
    }

    fn found(&mut self, cx: &mut Context<Self>) {
        if !self.lost {
            return;
        }
        log::info!("network: the connection is back");
        self.lost = false;
        self.still_offline = false;
        cx.notify();
        cx.refresh_windows();
        cx.emit(Reconnected);
    }
}

/// How much longer a manual check must show as busy after taking `elapsed`.
fn feedback_left(elapsed: Duration) -> Duration {
    FEEDBACK.saturating_sub(elapsed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FEEDBACK, feedback_left};

    #[test]
    fn a_check_that_fails_at_once_still_shows_busy_for_a_second() {
        assert_eq!(feedback_left(Duration::ZERO), FEEDBACK);
        assert_eq!(
            feedback_left(Duration::from_millis(300)),
            Duration::from_millis(700)
        );
        assert_eq!(feedback_left(FEEDBACK), Duration::ZERO);
        assert_eq!(feedback_left(Duration::from_secs(5)), Duration::ZERO);
    }
}
