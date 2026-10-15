//! CoW (Copy-on-Write) mutation logic for LedgerFS

// region:    --- Modules

mod object_store;
mod operations;
mod transaction;

#[cfg(test)]
mod property_tests;

pub use object_store::{InMemoryObjectStore, ObjectStore};
pub use operations::{
	Operation, apply_ops_to_root, cow_update_dir_entry, create_file, create_file_with_data, mkdir, read, read_inode,
	readdir, rename, unlink, write,
};
pub use transaction::{NewObject, TxnPlan};

// endregion: --- Modules
