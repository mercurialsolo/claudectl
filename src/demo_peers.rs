//! Binary-side demo helper: fake peer display info for the TUI peers panel.
//!
//! Kept in the binary (not `claudectl-tui::demo`) because it depends on
//! `crate::ui::peers::PeerDisplayInfo`, which still lives in `src/ui/`.
//! Once the `ui::peers` module migrates into `claudectl-tui` (follow-up to
//! #275), this helper can move there too.

#[cfg(feature = "relay")]
pub fn demo_peers(tick: u32) -> Vec<crate::ui::peers::PeerDisplayInfo> {
    let mut peers = vec![
        crate::ui::peers::PeerDisplayInfo {
            peer_id: "ci-runner-9d1e".into(),
            state: "connected".into(),
            trust: 0.82,
            units_sent: 42,
            units_received: 18,
            session_count: 3,
        },
        crate::ui::peers::PeerDisplayInfo {
            peer_id: "alice-mbp-f3a1".into(),
            state: if tick % 32 < 28 {
                "connecting".into()
            } else {
                "connected".into()
            },
            trust: 0.53,
            units_sent: 0,
            units_received: if tick % 32 >= 28 { 12 } else { 0 },
            session_count: if tick % 32 >= 28 { 2 } else { 0 },
        },
    ];

    // After tick 28, alice is connected and has received knowledge
    if tick % 32 >= 28 {
        peers[1].state = "connected".into();
        peers[1].trust = 0.53 + (tick % 32 - 28) as f64 * 0.01;
    }

    peers
}
