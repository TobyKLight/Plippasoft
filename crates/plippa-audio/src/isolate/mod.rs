//! Hostile media cannot abort a process (SECFIX_06 / PAR-01).
//!
//! Symphonia allocates buffers sized by attacker-controlled fields and a failed allocation is
//! `abort()`, which `catch_unwind` cannot stop. Every probe and decode of a file that came
//! from a stick, an upload or a download therefore runs in a **separate process**, the
//! `plippa-media-helper` binary of this crate:
//!
//! * the parent confines the path (`plippa_noubin::confine`), opens the file once, and hands the
//!   open descriptor over (inherited at [`proto::MEDIA_FD`]): the helper never opens a path;
//! * the helper runs under `RLIMIT_AS`, `RLIMIT_CPU`, `RLIMIT_CORE=0`, a small `RLIMIT_NOFILE`
//!   and `PR_SET_NO_NEW_PRIVS`, applied by the parent before `exec` and verified by the helper;
//! * the parent watches it with a stall rule (no output for N seconds) and kills and reaps it;
//! * a helper that dies by signal, exceeds a limit, stops answering or breaks the protocol
//!   marks the file ([`marks::CrashMarks`], keyed by path + mtime + size, persistent, on the
//!   device) and the file is [`AudioError::Unplayable`](crate::AudioError) until it changes.
//!
//! [`Isolation`] starts helpers; [`RemoteDecoder`] is a decoder that lives in one.

pub mod client;
pub mod limits;
pub mod marks;
pub mod proto;
pub mod server;

pub use client::{
    find_helper, Budgets, Failure, Isolation, RemoteDecoder, HELPER_ENV, HELPER_NAME,
};
pub use limits::Limits;
pub use marks::{CrashMarks, FileStamp, MAX_MARKS, STATE_DIR_ENV};
