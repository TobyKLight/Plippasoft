//! NFC R/W traits, mock, poll semantics, and associate write-through.

mod associate;
mod mock;
mod poll;
mod session;
mod traits;
mod types;

#[cfg(feature = "pn532")]
pub mod pn532;

pub use associate::{associate_playable_with_tag, AssociateError, AssociateParams, AssociateResult};
pub use mock::{MockNfc, MockTag};
pub use poll::{
    FakeClock, PollClock, PollScheduler, SteadyClock, MIN_POLL_HZ, MIN_POLL_INTERVAL,
};
pub use session::NfcSession;
pub use traits::NfcDevice;
pub use types::{NfcError, NfcEvent, TagInfo, TagType, TAG_READ_ONLY_MSG};
