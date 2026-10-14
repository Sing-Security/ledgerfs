//! Architecture-specific primitives.
//!
//! Compiled only for the x86-64 target with the `reader` feature, which is the
//! only configuration in which the ATA path exists — see `lib.rs`'s module
//! list and `ata_io.rs`.

pub(crate) mod x86_port;
