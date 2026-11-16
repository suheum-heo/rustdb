//! Optional phase timer for the update investigation.
//!
//! `start` returns `None` unless a test has enabled recording, so the commit
//! path does not sample the clock in ordinary runs.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Profile {
    pub compare: Duration,
    /// Building the table image, or auditing memory when that image is skipped.
    pub prepare: Duration,
    pub apply: Duration,
    pub wal_write: Duration,
    pub wal_sync: Duration,
    pub db_write: Duration,
    pub db_sync: Duration,
    pub wal_clear: Duration,
    #[cfg_attr(not(test), allow(dead_code))]
    pub frames: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub row_compares: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub heap_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub index_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub other_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub wal_bytes: u64,
}

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DATA: std::cell::Cell<Profile> = const { std::cell::Cell::new(Profile {
        compare: Duration::ZERO,
        prepare: Duration::ZERO,
        apply: Duration::ZERO,
        wal_write: Duration::ZERO,
        wal_sync: Duration::ZERO,
        db_write: Duration::ZERO,
        db_sync: Duration::ZERO,
        wal_clear: Duration::ZERO,
        frames: 0,
        row_compares: 0,
        heap_pages: 0,
        index_pages: 0,
        other_pages: 0,
        wal_bytes: 0,
    }) };
}

#[inline]
pub(crate) fn start() -> Option<Instant> {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            return Some(Instant::now());
        }
    }
    None
}

pub(crate) fn add_compare(mark: Option<Instant>) {
    add(|profile| &mut profile.compare, mark);
}

pub(crate) fn add_prepare(mark: Option<Instant>) {
    add(|profile| &mut profile.prepare, mark);
}

pub(crate) fn add_apply(mark: Option<Instant>) {
    add(|profile| &mut profile.apply, mark);
}

pub(crate) fn add_wal_write(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_write, mark);
}

pub(crate) fn add_wal_sync(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_sync, mark);
}

pub(crate) fn add_db_write(mark: Option<Instant>) {
    add(|profile| &mut profile.db_write, mark);
}

pub(crate) fn add_db_sync(mark: Option<Instant>) {
    add(|profile| &mut profile.db_sync, mark);
}

pub(crate) fn add_wal_clear(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_clear, mark);
}

pub(crate) fn note_frames(count: u32) {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            DATA.with(|data| {
                let mut profile = data.get();
                profile.frames = count;
                data.set(profile);
            });
        }
    }
    #[cfg(not(test))]
    {
        let _ = count;
    }
}

pub(crate) fn note_heap_page() {
    note_counter(|profile| &mut profile.heap_pages);
}

pub(crate) fn note_index_page() {
    note_counter(|profile| &mut profile.index_pages);
}

pub(crate) fn note_other_page() {
    note_counter(|profile| &mut profile.other_pages);
}

pub(crate) fn note_wal_bytes(bytes: usize) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        DATA.with(|data| {
            let mut profile = data.get();
            profile.wal_bytes = profile.wal_bytes.saturating_add(bytes);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = bytes;
    }
}

fn note_counter(slot: fn(&mut Profile) -> &mut u32) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        DATA.with(|data| {
            let mut profile = data.get();
            let current = *slot(&mut profile);
            *slot(&mut profile) = current.saturating_add(1);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = slot;
    }
}

pub(crate) fn note_row_compare() {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            DATA.with(|data| {
                let mut profile = data.get();
                profile.row_compares = profile.row_compares.saturating_add(1);
                data.set(profile);
            });
        }
    }
}

#[cfg(test)]
pub(crate) struct Guard;

#[cfg(test)]
impl Drop for Guard {
    fn drop(&mut self) {
        ENABLED.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
pub(crate) fn enable() -> Guard {
    ENABLED.with(|flag| flag.set(true));
    reset();
    Guard
}

#[cfg(test)]
pub(crate) fn reset() {
    DATA.with(|data| data.set(Profile::default()));
}

#[cfg(test)]
pub(crate) fn take() -> Profile {
    DATA.with(|data| data.get())
}

fn add(slot: fn(&mut Profile) -> &mut Duration, mark: Option<Instant>) {
    #[cfg(test)]
    {
        let Some(mark) = mark else {
            return;
        };
        DATA.with(|data| {
            let mut profile = data.get();
            *slot(&mut profile) += mark.elapsed();
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = (slot, mark);
    }
}
