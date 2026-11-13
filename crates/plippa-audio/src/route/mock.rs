//! Scriptable [`OutputControl`] for engine and UI tests (no audio server).

use super::{OutputControl, RouteError, RouteEvent, RouteIntent, SinkInfo};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct MockState {
    sinks: Vec<SinkInfo>,
    /// Successful `apply` calls, in order.
    applied: Vec<RouteIntent>,
    /// Every `apply` attempt, including failures.
    attempts: Vec<RouteIntent>,
    /// The `previous` argument of the last successful `apply`.
    last_previous: Option<RouteIntent>,
    events: VecDeque<RouteEvent>,
    /// When set, the next `apply` fails and the value is cleared.
    fail_next_apply: Option<String>,
}

/// In-memory output backend. Clone it to keep a handle for assertions after the
/// engine has taken ownership of one clone (as `MockSink` is used today).
#[derive(Debug, Clone, Default)]
pub struct MockOutputControl {
    state: Arc<Mutex<MockState>>,
}

impl MockOutputControl {
    pub fn new(sinks: Vec<SinkInfo>) -> Self {
        let control = Self::default();
        control.set_sinks(sinks);
        control
    }

    /// Replace the enumerated sink list (tests script discovery).
    pub fn set_sinks(&self, sinks: Vec<SinkInfo>) {
        self.lock().sinks = sinks;
    }

    /// Queue a server event for the next `poll`.
    pub fn push_event(&self, event: RouteEvent) {
        self.lock().events.push_back(event);
    }

    /// Make the next `apply` fail with `message` (one shot).
    pub fn fail_next_apply(&self, message: impl Into<String>) {
        self.lock().fail_next_apply = Some(message.into());
    }

    /// Successful `apply` calls, in order.
    pub fn applied(&self) -> Vec<RouteIntent> {
        self.lock().applied.clone()
    }

    /// Every `apply` attempt, including failures.
    pub fn attempts(&self) -> Vec<RouteIntent> {
        self.lock().attempts.clone()
    }

    pub fn applied_count(&self) -> usize {
        self.lock().applied.len()
    }

    pub fn last_applied(&self) -> Option<RouteIntent> {
        self.lock().applied.last().cloned()
    }

    /// The `previous` intent carried by the last successful `apply`.
    pub fn last_previous(&self) -> Option<RouteIntent> {
        self.lock().last_previous.clone()
    }

    pub fn clear(&self) {
        let mut state = self.lock();
        state.applied.clear();
        state.attempts.clear();
        state.last_previous = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock output control")
    }
}

impl OutputControl for MockOutputControl {
    fn sinks(&mut self) -> Result<Vec<SinkInfo>, RouteError> {
        Ok(self.lock().sinks.clone())
    }

    fn apply(
        &mut self,
        intent: &RouteIntent,
        previous: Option<&RouteIntent>,
    ) -> Result<(), RouteError> {
        let mut state = self.lock();
        state.attempts.push(intent.clone());
        if let Some(message) = state.fail_next_apply.take() {
            return Err(RouteError::Backend(message));
        }
        state.applied.push(intent.clone());
        state.last_previous = previous.cloned();
        Ok(())
    }

    fn poll(&mut self) -> Vec<RouteEvent> {
        self.lock().events.drain(..).collect()
    }
}
