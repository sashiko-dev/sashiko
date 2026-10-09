# GGC (`GTY(())`), Precompiled Headers, Containers, and Host Memory Safety

## 1. GCC Garbage Collector (`ggc`) and `GTY(())` Rules
- `tree`, `rtx`, `gimple *`, and `vec<..., va_gc> *` objects are allocated in GCC's garbage-collected heap (`ggc`).
- If a pointer to a GC object is stored in a global variable, static variable, or long-lived heap structure across a point where `ggc_collect ()` can run (between passes or during frontend parsing/instantiation), that variable/struct MUST be annotated with `GTY(())` (or registered as a GC root).
- Conversely, inside a `GTY(())` struct, any raw pointer to non-GC heap memory (allocated with `new`, `xmalloc`, `XNEW`) or non-`GTY` C++ class must be annotated with `GTY((skip))`, otherwise `gengtype` / PCH (`gt_pch_nx`) will crash when saving or restoring precompiled headers.

## 2. Container Reference Invalidation (`vec<>`, `auto_vec<>`, `hash_map<>`, `hash_table<>`)
- **Vector Reallocation**:
  - Taking a pointer or reference to an element (`T &elt = v[i];` or `T *p = &v.last ();`) and then calling `v.safe_push (...)`, `v.reserve (...)`, or a helper function that may push onto `v` invalidates `elt` / `p` if the vector reallocates (`use-after-free` ICE or memory corruption).
  - Read by index (`v[i]`) after the push, or copy the element value before pushing.
- **Hash Table / Hash Map Rehash**:
  - `hash_map::get_or_insert` and `hash_table::find_slot (..., INSERT)` can rehash the table and invalidate any pointers/references previously obtained from `get ()` or `find_slot ()` on the same table. Never hold a reference to a map slot across a recursive call or second `get_or_insert` on the same map.

## 3. Manual Memory Management (`bitmap`, `vec<T, va_heap>`, `XNEW`)
- `bitmap` allocated with `BITMAP_ALLOC (NULL)` must be freed with `BITMAP_FREE (b)` on all return paths (or use `auto_bitmap`).
- `vec<T>` (unlike `auto_vec<T>`) does not have a destructor and must be freed with `.release ()` on all return paths.
- Heap buffers allocated with `XNEW`, `XCNEW`, `XNEWVEC`, or `xmalloc` must be freed with `free ()` / `XDELETE` / `XDELETEVEC`.
