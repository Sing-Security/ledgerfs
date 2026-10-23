//! LedgerFS subsystems that exist but are not in the shipping path.
//!
//! FORMAT.md §44.1 splits the format into three production-readiness tiers. The
//! published `ledgerfs` crate is Tier A: the parts the host `Store` exercises
//! end to end on a live volume. **This** crate is Tier B: code that is
//! implemented and tested, but that nothing calls in the shipping path. The
//! document's own advice to a reader is to treat Tier B as "not available".
//!
//! It is a separate crate, marked `publish = false`, for exactly that reason.
//! A crate that a reader should not depend on has no business being installable,
//! and code that is real and tested has no business being deleted. Keeping the
//! two apart is what lets `ledgerfs` ship with no module, no dependency and no
//! error variant that its own code never reaches.
//!
//! # What is here
//!
//! - `dedupe`: the refcounted dedupe index and its locator/relocation types
//! - `snapshot`: snapshot pinning and its registry
//! - `scrubber`: background corruption scanning
//! - `gc`: garbage-collection *safety rules*, deciding what may be freed
//! - `cdc`: content-defined chunking
//!
//! GC *execution* is Tier C (not implemented) and is not here.
//!
//! # Depending on this
//!
//! Every module here is built on the published crate and reaches it through
//! `ledgerfs::`, one direction only: nothing in `ledgerfs` refers back. That is
//! what made the split mechanical, and it is worth preserving: a call from
//! the core into this crate would put Tier B back in the shipping path by
//! the back door and make the tiers lie again.

#![no_std]

extern crate alloc;

// region:    --- Modules

pub mod cdc;
pub mod dedupe;
pub mod error;
pub mod gc;
pub mod scrubber;
pub mod snapshot;

pub use error::{Error, Result};

// endregion: --- Modules
