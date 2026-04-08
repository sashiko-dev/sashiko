use std::sync::atomic::{AtomicUsize, Ordering};

pub static PENDING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static REVIEWING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static MESSAGES: AtomicUsize = AtomicUsize::new(0);
pub static PATCHSETS: AtomicUsize = AtomicUsize::new(0);

pub fn set_pending_patches(count: usize) {
    PENDING_PATCHES.store(count, Ordering::Relaxed);
}

pub fn set_reviewing_patches(count: usize) {
    REVIEWING_PATCHES.store(count, Ordering::Relaxed);
}

pub fn set_messages(count: usize) {
    MESSAGES.store(count, Ordering::Relaxed);
}

pub fn set_patchsets(count: usize) {
    PATCHSETS.store(count, Ordering::Relaxed);
}

pub fn get_pending_patches() -> usize {
    PENDING_PATCHES.load(Ordering::Relaxed)
}

pub fn get_reviewing_patches() -> usize {
    REVIEWING_PATCHES.load(Ordering::Relaxed)
}

pub fn get_messages() -> usize {
    MESSAGES.load(Ordering::Relaxed)
}

pub fn get_patchsets() -> usize {
    PATCHSETS.load(Ordering::Relaxed)
}
