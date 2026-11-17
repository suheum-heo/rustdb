//! Page-oriented database file.
//!
//! The pager, record codec, and heap layout do not know about SQL text. The
//! executor mutates the in-memory catalog, then asks the store to publish a
//! new file image.

mod btree;
mod edit;
mod lock;
mod page;
mod pager;
mod profile;
mod record;
#[cfg(test)]
mod reliability;
mod store;
mod wal;

#[cfg(test)]
pub(crate) use btree::ForceAncestorRewrite;
#[cfg(test)]
pub(crate) use btree::ForceChainVerify;
#[cfg(test)]
pub(crate) use btree::ForceDescentVerify;
#[cfg(test)]
pub(crate) use btree::ForceGroupedDelete;
#[cfg(test)]
pub(crate) use btree::ForceLatePresence;
#[cfg(test)]
pub(crate) use btree::ForcePriorPlan;
#[cfg(test)]
pub(crate) use edit::ForcePerRowUnlink;
#[cfg(test)]
pub(crate) use page::{ForceChecksumRecheck, TYPE_HEAP, init_slotted, insert_cell, seal};
pub use page::{MAX_RECORD_LEN, PAGE_SIZE};
pub(crate) use page::{Page, cell, live_slots, page_id_of, verify};
#[cfg(test)]
pub(crate) use profile::{
    Profile, SuspendSync, enable as enable_phases, reset as reset_phases,
    set_enabled as set_phases_enabled, take as take_phases,
};
pub(crate) use profile::{Timer, add_memory, add_parse, add_plan, start as phase_start};
#[cfg(test)]
pub(crate) use record::{NULL_LINK, StoredRecord, encode_record};
pub(crate) use record::{decode_catalog, decode_index, decode_record, ensure_record_fits};
pub(crate) use store::{IndexImage, MemoryRows, Store, TableImage};

pub(crate) fn publish_timer() -> Option<std::time::Instant> {
    profile::start()
}

pub(crate) fn add_publish_prepare(mark: Option<std::time::Instant>) {
    profile::add_prepare(mark);
}
#[cfg(test)]
pub(crate) use wal::Inject;
pub(crate) use wal::{LogClass, StepError, classify_log};
#[cfg(test)]
pub(crate) use wal::{Plan, encode_committed};
