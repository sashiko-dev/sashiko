+++
title = "MM Allocation"
paths = ["mm/page_alloc.c", "mm/slub.c", "mm/slab_common.c", "mm/vmalloc.c"]
symbols = ["alloc_pages", "__GFP_*", "kmalloc", "kzalloc", "kmem_cache_*", "slub", "vmalloc", "zone_watermark", "mempool", "memblock", "__get_free_page", "__get_free_pages", "get_zeroed_page", "free_page", "free_pages", "ARCH_KMALLOC_MINALIGN", "KMALLOC_MAX_CACHE_SIZE"]
+++
# MM Allocation: maintainer notes

These notes cover maintainer preferences for code using the kernel memory allocators (page allocator, slab/kmalloc, vmalloc, mempool, memblock).

- Prefer the scoped memalloc_nofs_save()/memalloc_noio_save() APIs over passing GFP_NOFS/GFP_NOIO flags.
- New __GFP_ACCOUNT usage should be consistent with the surrounding code.
- Functions that run only early in boot (e.g. marked __init) usually need not handle allocation failures gracefully; an allocation failure there typically means the system cannot boot anyway, so complex error handling, cleanup logic or returning -ENOMEM is often unnecessary dead code and should not be requested.
