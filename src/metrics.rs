use std::sync::atomic::{AtomicUsize, Ordering};

pub static PENDING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static REVIEWING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static MESSAGES: AtomicUsize = AtomicUsize::new(0);
pub static PATCHSETS: AtomicUsize = AtomicUsize::new(0);
pub static REPO_PACKS: AtomicUsize = AtomicUsize::new(0);
pub static REPO_PACK_BYTES: AtomicUsize = AtomicUsize::new(0);

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

/// Records the size of the review repository's pack directory.  With
/// git's own maintenance turned off, nothing reclaims packs, so this
/// is where that cost shows up.
pub fn set_repo_packs(count: usize, bytes: u64) {
    REPO_PACKS.store(count, Ordering::Relaxed);
    REPO_PACK_BYTES.store(bytes as usize, Ordering::Relaxed);
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

pub fn get_repo_packs() -> usize {
    REPO_PACKS.load(Ordering::Relaxed)
}

pub fn get_repo_pack_bytes() -> usize {
    REPO_PACK_BYTES.load(Ordering::Relaxed)
}
