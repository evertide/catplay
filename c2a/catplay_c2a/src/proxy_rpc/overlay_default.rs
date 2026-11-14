use crate::{
    proxy_rpc::{OverlayPeerState, OverlayPolicy},
    ui::UiState,
};
use catplay_bt::BluezManager;
use catplay_util::{EventReconciler, EventSleeper, EventToken, LazyAsync};
use log::{debug, warn};
use std::time::Duration;

pub struct OverlayPolicyDefault {
    bluez: LazyAsync<BluezManager>,
    peer: OverlayPeerState,
    overlay: Option<UiState>,
    inviting_iphone: bool,
}

/// The waiting screen while no iPhone session runs: a pairing request wins; once the wireless side
/// invites the iPhone the connecting phase shows, before that (the dongle is still starting) the
/// waiting screen.
fn waiting_screen(pairing: Option<UiState>, inviting_iphone: bool) -> UiState {
    match (pairing, inviting_iphone) {
        (Some(pairing), _) => pairing,
        // Static dots: the screen is redrawn only when the phase changes.
        (None, true) => UiState::Connecting {
            device: "iPhone".into(),
            ticks: 2,
        },
        (None, false) => UiState::WaitingForConnection,
    }
}

impl OverlayPolicyDefault {
    pub fn new() -> Self {
        Self {
            bluez: LazyAsync::new(Self::start_pin_agent),
            peer: OverlayPeerState::Unconnected,
            overlay: None,
            inviting_iphone: false,
        }
    }

    pub fn set_inviting_iphone(&mut self, inviting: bool) {
        self.inviting_iphone = inviting;
    }
    async fn start_pin_agent() -> BluezManager {
        loop {
            let mut mgr = BluezManager::new();
            match mgr.register_pin_agent().await {
                Ok(()) => {
                    if let Err(err) = mgr.set_discoverable("hci0", true).await {
                        warn!("Failed to set BlueZ adapter discoverable: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    if let Err(err) = mgr.set_pairable("hci0", true).await {
                        warn!("Failed to set BlueZ adapter pairable: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return mgr;
                }
                Err(err) => {
                    debug!("BlueZ agent not available: {err}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

impl OverlayPolicy for OverlayPolicyDefault {
    fn overlay(&mut self) -> Option<UiState> {
        if self.peer != OverlayPeerState::Unconnected {
            return None;
        }

        let pairing = self
            .bluez
            .as_ref()
            .as_ref()
            .and_then(|bluez| bluez.get_pairing_request())
            .map(|req| UiState::Pairing {
                device: req.remote_name.unwrap_or("unknown".into()),
                pin: format!("{}", req.passkey),
            });

        Some(waiting_screen(pairing, self.inviting_iphone))
    }

    fn on_hid_interact(&mut self) -> bool {
        if let Some(req) = self
            .bluez
            .as_ref()
            .as_ref()
            .and_then(|bluez| bluez.get_pairing_request())
        {
            req.accept();
            return false;
        }

        true
    }

    fn set_peer_state(&mut self, state: OverlayPeerState) {
        self.peer = state;
    }
}

impl EventReconciler for OverlayPolicyDefault {
    type Error = ();

    async fn reconcile(&mut self) -> Result<(), Self::Error> {
        self.overlay = self.overlay();
        Ok(())
    }
}

impl EventSleeper for OverlayPolicyDefault {
    async fn sleep(&mut self) -> Option<EventToken> {
        self.bluez.sleep().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_screen_follows_the_wireless_phase() {
        assert_eq!(waiting_screen(None, false), UiState::WaitingForConnection);
        assert!(matches!(waiting_screen(None, true), UiState::Connecting { .. }));
        assert_eq!(waiting_screen(None, true), waiting_screen(None, true), "same phase, same state: no redraw");

        let pairing = UiState::Pairing {
            device: "iPhone".into(),
            pin: "1234".into(),
        };
        assert_eq!(waiting_screen(Some(pairing.clone()), false), pairing);
        assert_eq!(waiting_screen(Some(pairing.clone()), true), pairing, "pairing wins");
    }

    #[test]
    fn policy_shows_the_phase_only_without_a_session() {
        let mut policy = OverlayPolicyDefault::new();
        assert_eq!(policy.overlay(), Some(UiState::WaitingForConnection));
        policy.set_inviting_iphone(true);
        assert!(matches!(policy.overlay(), Some(UiState::Connecting { .. })));
        policy.set_peer_state(OverlayPeerState::ConnectedWithoutScreen);
        assert_eq!(policy.overlay(), None);
        policy.set_peer_state(OverlayPeerState::Unconnected);
        policy.set_inviting_iphone(false);
        assert_eq!(policy.overlay(), Some(UiState::WaitingForConnection));
    }
}
