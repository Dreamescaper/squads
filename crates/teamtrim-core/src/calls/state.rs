//! The call state machine (milestone **M9**).
//!
//! This is the UI-facing lifecycle of a call, deliberately **independent of the
//! vendored [`ost::calling::CallState`]** (which is a coarse signalling enum:
//! `Idle`/`Ringing`/`Accepting`/`Connected`/`Ended`). The UI needs to
//! distinguish an *outgoing* call we are placing from an *incoming* one that is
//! ringing, to know why a call ended, and to reject nonsensical transitions.
//!
//! ```text
//!                 place()                      connected()
//!   Idle ───────────────▶ Placing ─┐
//!     │                             ├──▶ Connecting ──▶ Connected
//!     └──── ring() ──────▶ Ringing ─┘         │              │
//!                                             ▼              ▼
//!                                          Ended ◀───────────┘  (end(reason))
//! ```
//!
//! Every transition is explicit and checked: an illegal move (e.g. `Connected`
//! back to `Ringing`, or ending a call that never started) returns a
//! [`TransitionError`] rather than silently corrupting the state. [`CallMachine`]
//! also records the [`CallDirection`] established by `place`/`ring`.

use std::fmt;

use crate::model::CallDirection;

/// Why a call ended.
///
/// `Failed` carries a short, non-sensitive reason (an error string, never a
/// token) so the UI can show it; the rest are the normal terminal outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// We hung up a connected call.
    LocalHangup,
    /// The remote party hung up.
    RemoteHangup,
    /// We declined an incoming call before answering.
    Declined,
    /// An incoming call rang out unanswered.
    Missed,
    /// Placing or accepting the call failed.
    Failed(String),
    /// We cancelled a call that had not connected.
    Cancelled,
}

impl EndReason {
    /// A short, stable label (for logs, tests and the UI status line).
    pub fn as_str(&self) -> &str {
        match self {
            EndReason::LocalHangup => "local_hangup",
            EndReason::RemoteHangup => "remote_hangup",
            EndReason::Declined => "declined",
            EndReason::Missed => "missed",
            EndReason::Failed(_) => "failed",
            EndReason::Cancelled => "cancelled",
        }
    }
}

/// The UI-facing call lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CallState {
    /// No call.
    #[default]
    Idle,
    /// An outgoing call is being set up (conversation created / SDP joined).
    Placing,
    /// An incoming call is ringing.
    Ringing,
    /// The call is connecting (we accepted, or the remote answered).
    Connecting,
    /// Media is up.
    Connected,
    /// The call is over, with the reason it ended.
    Ended {
        /// Why the call ended.
        reason: EndReason,
    },
}

impl CallState {
    /// A short label for the UI status line.
    pub fn label(&self) -> &str {
        match self {
            CallState::Idle => "Idle",
            CallState::Placing => "Calling…",
            CallState::Ringing => "Incoming call",
            CallState::Connecting => "Connecting…",
            CallState::Connected => "In call",
            CallState::Ended { .. } => "Call ended",
        }
    }

    /// Whether a call is in progress (not `Idle` and not `Ended`).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            CallState::Placing | CallState::Ringing | CallState::Connecting | CallState::Connected
        )
    }

    /// Whether the state is terminal (`Ended`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, CallState::Ended { .. })
    }

    /// The [`EndReason`] when the call has ended.
    pub fn end_reason(&self) -> Option<&EndReason> {
        match self {
            CallState::Ended { reason } => Some(reason),
            _ => None,
        }
    }
}

/// An illegal state transition was requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    /// The state the machine was in.
    pub from: CallState,
    /// The state the caller tried to move to.
    pub to: CallState,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal call transition {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for TransitionError {}

/// The call state machine: a [`CallState`] plus the [`CallDirection`] it was
/// established with.
///
/// Pure and clock-free, so every transition is unit-tested with no I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallMachine {
    state: CallState,
    direction: Option<CallDirection>,
}

impl CallMachine {
    /// A fresh, idle machine.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    pub fn state(&self) -> &CallState {
        &self.state
    }

    /// The direction of the current/last call, when one was established.
    pub fn direction(&self) -> Option<CallDirection> {
        self.direction
    }

    /// Begin an **outgoing** call. Allowed from `Idle` or a previous `Ended`.
    pub fn place(&mut self) -> Result<(), TransitionError> {
        self.begin(CallState::Placing, CallDirection::Outgoing)
    }

    /// Begin an **incoming** call (ringing). Allowed from `Idle` or `Ended`.
    pub fn ring(&mut self) -> Result<(), TransitionError> {
        self.begin(CallState::Ringing, CallDirection::Incoming)
    }

    fn begin(&mut self, to: CallState, direction: CallDirection) -> Result<(), TransitionError> {
        if matches!(self.state, CallState::Idle | CallState::Ended { .. }) {
            self.direction = Some(direction);
            self.state = to;
            Ok(())
        } else {
            Err(self.reject(to))
        }
    }

    /// Move to `Connecting`: from `Placing` (the remote answered / we joined) or
    /// from `Ringing` (we accepted).
    pub fn connecting(&mut self) -> Result<(), TransitionError> {
        match self.state {
            CallState::Placing | CallState::Ringing => {
                self.state = CallState::Connecting;
                Ok(())
            }
            _ => Err(self.reject(CallState::Connecting)),
        }
    }

    /// Move to `Connected`: only from `Connecting`.
    pub fn connected(&mut self) -> Result<(), TransitionError> {
        match self.state {
            CallState::Connecting => {
                self.state = CallState::Connected;
                Ok(())
            }
            _ => Err(self.reject(CallState::Connected)),
        }
    }

    /// End the call with `reason`. Allowed from any non-`Idle` state; ending an
    /// already-ended call replaces the reason (a later, more specific reason —
    /// e.g. a remote hangup after a local one — wins).
    pub fn end(&mut self, reason: EndReason) -> Result<(), TransitionError> {
        if matches!(self.state, CallState::Idle) {
            return Err(self.reject(CallState::Ended {
                reason: reason.clone(),
            }));
        }
        self.state = CallState::Ended { reason };
        Ok(())
    }

    /// Hard reset to `Idle`, clearing the direction. Always succeeds; used when
    /// the UI dismisses a finished call.
    pub fn reset(&mut self) {
        self.state = CallState::Idle;
        self.direction = None;
    }

    fn reject(&self, to: CallState) -> TransitionError {
        TransitionError {
            from: self.state.clone(),
            to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_machine_is_idle_and_directionless() {
        let m = CallMachine::new();
        assert_eq!(m.state(), &CallState::Idle);
        assert!(m.direction().is_none());
        assert!(!m.state().is_active());
        assert!(!m.state().is_terminal());
        assert_eq!(CallState::Idle.label(), "Idle");
    }

    #[test]
    fn outgoing_flow_place_connecting_connected_end() {
        let mut m = CallMachine::new();
        m.place().expect("place");
        assert_eq!(m.state(), &CallState::Placing);
        assert_eq!(m.direction(), Some(CallDirection::Outgoing));
        assert!(m.state().is_active());

        m.connecting().expect("connecting");
        assert_eq!(m.state(), &CallState::Connecting);
        m.connected().expect("connected");
        assert_eq!(m.state(), &CallState::Connected);

        m.end(EndReason::LocalHangup).expect("end");
        assert!(m.state().is_terminal());
        assert_eq!(m.state().end_reason(), Some(&EndReason::LocalHangup));
        assert_eq!(CallState::Connected.label(), "In call");

        // A reset returns to idle and clears the direction.
        m.reset();
        assert_eq!(m.state(), &CallState::Idle);
        assert!(m.direction().is_none());
    }

    #[test]
    fn incoming_flow_ring_connecting_connected() {
        let mut m = CallMachine::new();
        m.ring().expect("ring");
        assert_eq!(m.state(), &CallState::Ringing);
        assert_eq!(m.direction(), Some(CallDirection::Incoming));
        assert_eq!(CallState::Ringing.label(), "Incoming call");

        m.connecting().expect("accept");
        m.connected().expect("connected");
        assert_eq!(m.state(), &CallState::Connected);
        m.end(EndReason::RemoteHangup).expect("end");
        assert_eq!(m.state().end_reason(), Some(&EndReason::RemoteHangup));
    }

    #[test]
    fn declined_and_missed_are_terminal_from_ringing() {
        let mut declined = CallMachine::new();
        declined.ring().unwrap();
        declined.end(EndReason::Declined).unwrap();
        assert_eq!(declined.state().end_reason(), Some(&EndReason::Declined));

        let mut missed = CallMachine::new();
        missed.ring().unwrap();
        missed.end(EndReason::Missed).unwrap();
        assert_eq!(missed.state().end_reason(), Some(&EndReason::Missed));
    }

    #[test]
    fn a_new_call_is_allowed_after_ended() {
        let mut m = CallMachine::new();
        m.place().unwrap();
        m.end(EndReason::Cancelled).unwrap();
        // A second call restarts from the terminal state.
        m.ring().unwrap();
        assert_eq!(m.state(), &CallState::Ringing);
        assert_eq!(m.direction(), Some(CallDirection::Incoming));
    }

    #[test]
    fn ending_an_ended_call_replaces_the_reason() {
        let mut m = CallMachine::new();
        m.place().unwrap();
        m.end(EndReason::Cancelled).unwrap();
        m.end(EndReason::RemoteHangup).unwrap();
        assert_eq!(m.state().end_reason(), Some(&EndReason::RemoteHangup));
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        // Cannot connect or end from Idle.
        let mut idle = CallMachine::new();
        assert!(idle.connecting().is_err());
        assert!(idle.connected().is_err());
        assert!(idle.end(EndReason::Missed).is_err());
        assert_eq!(idle.state(), &CallState::Idle, "state unchanged");

        // Cannot connect straight from Placing without `connecting()`.
        let mut placing = CallMachine::new();
        placing.place().unwrap();
        assert!(placing.connected().is_err());
        assert_eq!(placing.state(), &CallState::Placing);

        // Cannot start a second call while one is active.
        assert!(placing.ring().is_err());
        assert!(placing.place().is_err());

        // Connected cannot go back to Ringing/Placing/Connecting.
        let mut connected = CallMachine::new();
        connected.place().unwrap();
        connected.connecting().unwrap();
        connected.connected().unwrap();
        assert!(connected.ring().is_err());
        assert!(connected.place().is_err());
        assert!(connected.connecting().is_err());

        // The error reports both ends of the failed transition.
        let err = connected.ring().unwrap_err();
        assert_eq!(err.from, CallState::Connected);
        assert_eq!(err.to, CallState::Ringing);
        assert!(format!("{err}").contains("illegal call transition"));
    }

    #[test]
    fn end_reason_labels_are_stable() {
        assert_eq!(EndReason::LocalHangup.as_str(), "local_hangup");
        assert_eq!(EndReason::RemoteHangup.as_str(), "remote_hangup");
        assert_eq!(EndReason::Declined.as_str(), "declined");
        assert_eq!(EndReason::Missed.as_str(), "missed");
        assert_eq!(EndReason::Cancelled.as_str(), "cancelled");
        assert_eq!(EndReason::Failed("boom".into()).as_str(), "failed");
    }
}
