//! SECFIX_01 (SRV-02): the HTTP layer's limits in one struct.
//!
//! [`HttpLimits::default`] is the production set. [`serve_with_limits`](crate::serve_with_limits)
//! takes one; [`serve`](crate::serve) and [`serve_until`](crate::serve_until) use the default.
//! Tests pass shortened timeouts to prove slow-network behaviour over real loopback sockets.
//!
//! `ServerConfig` deliberately gains no fields (fifteen integration test files build it as a
//! struct literal): the limits travel in this struct and the entry point instead.

use crate::admission::{AdmissionLimits, Connections};
use std::sync::Arc;

pub use tiny_http::Limits as WireLimits;

/// Everything the HTTP layer limits, plus the registry of live connections.
#[derive(Clone)]
pub struct HttpLimits {
    /// Request line / header caps and the socket timeouts (see [`WireLimits`]).
    pub wire: WireLimits,
    /// Connection admission and the live-connection registry. Clone this before calling
    /// `serve_with_limits` to keep a handle for listing or force-closing connections
    /// (`connections.close_matching(|c| !c.is_loopback())`). Its caps are fixed when it is
    /// created: build it with [`HttpLimits::with_admission`] to change them.
    pub connections: Connections,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self::with_admission(WireLimits::default(), AdmissionLimits::default())
    }
}

impl HttpLimits {
    pub fn with_admission(wire: WireLimits, admission: AdmissionLimits) -> Self {
        Self {
            wire,
            connections: Connections::new(admission),
        }
    }

    /// Handler threads that may run at once: one request per admitted connection (a connection
    /// is not read again until its request has been answered), so the connection cap bounds it.
    pub(crate) fn max_handlers(&self) -> usize {
        self.connections.limits().max_total.max(1)
    }

    pub(crate) fn gate(&self) -> Arc<dyn tiny_http::ConnectionGate> {
        Arc::new(self.connections.clone())
    }
}
