//! Page-oriented database file.
//!
//! The pager, record codec, and heap layout do not know about SQL text. The
//! executor mutates the in-memory catalog, then asks the store to publish a
//! new file image.

mod edit;
mod lock;
mod page;
mod pager;
mod profile;
mod record;
mod store;
mod wal;

pub use page::{MAX_RECORD_LEN, PAGE_SIZE};
pub(crate) use record::ensure_record_fits;
pub(crate) use store::{Store, TableImage};
#[cfg(test)]
pub(crate) use wal::Inject;
pub(crate) use wal::StepError;
