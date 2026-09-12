// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Finite state machine for the GUI window lifecycle.
//!
//! Implements the CLAP-spec-compliant state machine:
//! `Hidden → ShowRequested → Active → HideRequested → Hidden → Destroyed`
//!
//! This guarantees that the plugin:
//! - Never shows the window before the host calls `show()`.
//! - Never hides the window before the host calls `hide()`.
//! - Always notifies the host via `HostGui::closed()` when the user closes the window.
//! - Prevents invalid transitions (double-show, hide-before-activate, etc.).
//!
//! # Synchronous window-hide contract
//!
//! Slint's `Window::hide()` is dispatched to the GUI event loop and there is no
//! asynchronous "the window is now unmapped" notification available today, so
//! `hide()` drives `HideRequested → Hidden` immediately after the dispatch is
//! accepted. `GuiEvent::WindowHidden` therefore models the dispatch being
//! accepted, not a backend round-trip. A future native-embedding backend may
//! restore a genuinely asynchronous confirmation.
//!
//! Hiding is also accepted from `ShowRequested`: a host may call `hide()` before
//! the backend has confirmed `WindowReady` (or without any backend window at
//! all), and that request must never leave the window stuck visible.
//!
//! # State transition table
//!
//! | Current State   | Event           | Next State      | Valid? |
//! |-----------------|-----------------|-----------------|--------|
//! | Hidden          | show()          | ShowRequested   | ✅     |
//! | ShowRequested   | window_ready    | Active          | ✅     |
//! | ShowRequested   | hide()          | HideRequested   | ✅     |
//! | Active          | hide()          | HideRequested   | ✅     |
//! | HideRequested   | window_hidden   | Hidden          | ✅     |
//! | ShowRequested   | window_failed   | Hidden          | ✅     |
//! | ShowRequested   | user_closed     | Hidden          | ✅     |
//! | Active          | user_closed     | Hidden          | ✅     |
//! | *               | destroy()       | Destroyed       | ✅     |
//! | Destroyed       | any()           | Destroyed       | ✅ (no-op) |

use clack_plugin::plugin::PluginError;

/// Finite state for the GUI window lifecycle.
///
/// # State transition table
///
/// | Current State   | Event           | Next State      | Valid? |
/// |-----------------|-----------------|-----------------|--------|
/// | Hidden          | show()          | ShowRequested   | ✅     |
/// | ShowRequested   | window_ready    | Active          | ✅     |
/// | ShowRequested   | hide()          | HideRequested   | ✅     |
/// | Active          | hide()          | HideRequested   | ✅     |
/// | HideRequested   | window_hidden   | Hidden          | ✅     |
/// | ShowRequested   | window_failed   | Hidden          | ✅     |
/// | ShowRequested   | user_closed     | Hidden          | ✅     |
/// | Active          | user_closed     | Hidden          | ✅     |
/// | *               | destroy()       | Destroyed       | ✅     |
/// | Destroyed       | any()           | Destroyed       | ✅ (no-op) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuiLifecycle {
    /// Window has not been created or is hidden.
    /// Initial state after `create()`.
    #[default]
    Hidden,
    /// Host has called `show()`. Window creation/display is in progress.
    /// The GUI thread will transition to `Active` once the window is mapped.
    ShowRequested,
    /// Window is visible and receiving events.
    Active,
    /// Host has called `hide()`. Window is being unmapped/closed.
    /// The GUI thread will transition to `Hidden` once complete.
    HideRequested,
    /// Window has been destroyed (teardown). Terminal state.
    Destroyed,
}

impl GuiLifecycle {
    /// Validates and executes a transition, returning `Err` if the transition is illegal.
    pub fn transition(&mut self, event: GuiEvent) -> Result<(), PluginError> {
        let next = match (*self, event) {
            (GuiLifecycle::ShowRequested, GuiEvent::WindowReady) => GuiLifecycle::Active,
            (GuiLifecycle::HideRequested, GuiEvent::WindowHidden) => GuiLifecycle::Hidden,

            // show → host requested visibility
            (GuiLifecycle::Hidden, GuiEvent::Show) => GuiLifecycle::ShowRequested,

            // hide → host requested the window to be hidden
            (GuiLifecycle::Active, GuiEvent::Hide) => GuiLifecycle::HideRequested,

            // hide before the backend confirmed WindowReady: the host must still
            // be able to take the window down without the FSM rejecting it.
            (GuiLifecycle::ShowRequested, GuiEvent::Hide) => GuiLifecycle::HideRequested,

            // user closed the window externally (WM close button, X button, etc.)
            // Valid from ShowRequested (window appeared then user immediately closed it)
            // or Active (normal close).
            (GuiLifecycle::ShowRequested, GuiEvent::UserClosed)
            | (GuiLifecycle::Active, GuiEvent::UserClosed) => GuiLifecycle::Hidden,

            // window creation failed before reaching Active
            (GuiLifecycle::ShowRequested, GuiEvent::WindowFailed) => GuiLifecycle::Hidden,

            // destroy is valid from any state (terminal)
            (_, GuiEvent::Destroy) => GuiLifecycle::Destroyed,

            // Destroyed is terminal — all events are no-ops
            (GuiLifecycle::Destroyed, _) => GuiLifecycle::Destroyed,

            // Illegal transitions
            (current, event) => {
                let err: &'static str = Box::leak(
                    format!(
                        "GUI lifecycle: illegal transition {:?} -> {:?}",
                        current, event
                    )
                    .into_boxed_str(),
                );
                log::warn!("{err}");
                return Err(PluginError::Message(err));
            }
        };
        *self = next;
        Ok(())
    }

    /// Returns `true` if the state machine is in a state where `show()` is allowed.
    pub fn can_show(&self) -> bool {
        matches!(self, GuiLifecycle::Hidden)
    }

    /// Returns `true` if the state machine is in a state where `hide()` is allowed.
    pub fn can_hide(&self) -> bool {
        matches!(self, GuiLifecycle::Active)
    }

    /// Returns `true` if the window is currently active (visible and receiving events).
    pub fn is_active(&self) -> bool {
        matches!(self, GuiLifecycle::Active)
    }

    /// Returns `true` if the window has been destroyed (terminal state).
    pub fn is_destroyed(&self) -> bool {
        matches!(self, GuiLifecycle::Destroyed)
    }
}

/// Events that drive the GUI lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiEvent {
    /// Host called `show()` on the plugin GUI.
    Show,
    /// Host called `hide()` on the plugin GUI.
    Hide,
    /// Host called `destroy()` — terminal teardown.
    Destroy,
    /// The window backend confirmed that the window is visible.
    WindowReady,
    /// The window backend confirmed that the window was hidden/unmapped.
    WindowHidden,
    /// The user closed the window via the window manager (X button, Alt-F4, etc.).
    UserClosed,
    /// Window creation failed (X11 connection failed, timeout, etc.).
    WindowFailed,
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn test_happy_path_hidden_to_active() {
        let mut state = GuiLifecycle::Hidden;
        state.transition(GuiEvent::Show).unwrap();
        assert_eq!(state, GuiLifecycle::ShowRequested);
        state.transition(GuiEvent::WindowReady).unwrap();
        assert_eq!(state, GuiLifecycle::Active);
    }

    #[test]
    fn test_happy_path_active_to_hidden() {
        let mut state = GuiLifecycle::Active;
        state.transition(GuiEvent::Hide).unwrap();
        assert_eq!(state, GuiLifecycle::HideRequested);
        state.transition(GuiEvent::WindowHidden).unwrap();
        assert_eq!(state, GuiLifecycle::Hidden);
    }

    #[test]
    fn test_user_closes_from_active() {
        let mut state = GuiLifecycle::Active;
        state.transition(GuiEvent::UserClosed).unwrap();
        assert_eq!(state, GuiLifecycle::Hidden);
    }

    #[test]
    fn test_user_closes_from_show_requested() {
        let mut state = GuiLifecycle::ShowRequested;
        state.transition(GuiEvent::UserClosed).unwrap();
        assert_eq!(state, GuiLifecycle::Hidden);
    }

    #[test]
    fn test_destroy_from_any_state() {
        for start in [
            GuiLifecycle::Hidden,
            GuiLifecycle::ShowRequested,
            GuiLifecycle::Active,
            GuiLifecycle::HideRequested,
        ] {
            let mut state = start;
            state.transition(GuiEvent::Destroy).unwrap();
            assert_eq!(state, GuiLifecycle::Destroyed);
        }
    }

    #[test]
    fn test_destroyed_is_terminal() {
        let mut state = GuiLifecycle::Destroyed;
        state.transition(GuiEvent::Show).unwrap(); // no-op in Destroyed
        assert_eq!(state, GuiLifecycle::Destroyed);
        state.transition(GuiEvent::Hide).unwrap();
        assert_eq!(state, GuiLifecycle::Destroyed);
        state.transition(GuiEvent::UserClosed).unwrap();
        assert_eq!(state, GuiLifecycle::Destroyed);
    }

    #[test]
    fn test_illegal_transitions() {
        // Can't show when already active
        assert!(GuiLifecycle::Active.transition(GuiEvent::Show).is_err());

        // Can't hide from hidden
        assert!(GuiLifecycle::Hidden.transition(GuiEvent::Hide).is_err());

        // Can't show twice
        let mut state = GuiLifecycle::ShowRequested;
        assert!(state.transition(GuiEvent::Show).is_err());

        // Can't window_ready from hidden
        assert!(
            GuiLifecycle::Hidden
                .transition(GuiEvent::WindowReady)
                .is_err()
        );

        // Can't window_hidden from active
        assert!(
            GuiLifecycle::Active
                .transition(GuiEvent::WindowHidden)
                .is_err()
        );
    }

    /// Regression guard for Finding 1: when the backend never reports
    /// `WindowReady`, the FSM parks in `ShowRequested`, and a subsequent host
    /// `hide()` must still be accepted so the window is actually taken down.
    ///
    /// The production wiring that delivers `WindowReady`/`UserClosed` is
    /// exercised by the `gui_lifecycle_integration_test` module.
    #[test]
    fn test_show_without_window_ready_then_hide_is_accepted() {
        let mut state = GuiLifecycle::Hidden;
        state.transition(GuiEvent::Show).unwrap();
        assert_eq!(state, GuiLifecycle::ShowRequested);

        state
            .transition(GuiEvent::Hide)
            .expect("Hide from ShowRequested must be accepted");
        assert_eq!(state, GuiLifecycle::HideRequested);

        state
            .transition(GuiEvent::WindowHidden)
            .expect("HideRequested must accept the synchronous WindowHidden signal");
        assert_eq!(state, GuiLifecycle::Hidden);
    }

    #[test]
    fn test_window_failed_recovery() {
        let mut state = GuiLifecycle::ShowRequested;
        state.transition(GuiEvent::WindowFailed).unwrap();
        assert_eq!(state, GuiLifecycle::Hidden);
        // Can retry show after failure
        state.transition(GuiEvent::Show).unwrap();
        assert_eq!(state, GuiLifecycle::ShowRequested);
    }

    #[test]
    fn test_can_show_can_hide_helpers() {
        assert!(GuiLifecycle::Hidden.can_show());
        assert!(!GuiLifecycle::Active.can_show());
        assert!(GuiLifecycle::Active.can_hide());
        assert!(!GuiLifecycle::Hidden.can_hide());
        assert!(GuiLifecycle::Active.is_active());
        assert!(!GuiLifecycle::Hidden.is_active());
    }
}
