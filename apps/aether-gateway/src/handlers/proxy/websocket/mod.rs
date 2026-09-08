//! Shared infrastructure for public AI WebSocket bridges.
//!
//! Protocol adapters live below [`responses`].  This layer deliberately owns
//! only transport concerns that are common to future adapters: authenticated
//! upgrade admission, connection limits, upstream handshakes, and frame
//! conversion.  It does not interpret provider events or make routing
//! decisions.

pub(crate) mod ingress;
pub(crate) mod live;
pub(crate) mod realtime;
pub(crate) mod responses;
pub(crate) mod session;
pub(crate) mod transport;
