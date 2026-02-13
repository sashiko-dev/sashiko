# RCU Subsystem Details

## RCU Read-Side Critical Sections

- `rcu_read_lock()` / `rcu_read_unlock()` delimit read-side critical sections
- No blocking or sleeping inside classic RCU read sections
- Nesting `rcu_read_lock()` calls is safe
- Preemption of read sections is allowed under `CONFIG_PREEMPT_RCU`

## Publishing and Reading

- `rcu_assign_pointer(p, v)`: publishes a pointer with release semantics (`smp_store_release`), ensuring prior initialization is visible to readers
- `rcu_dereference(p)`: loads a pointer with dependency ordering (`READ_ONCE`), ensuring subsequent accesses through the pointer see published data
- These form a pair: the release in `rcu_assign_pointer()` orders with the dependency in `rcu_dereference()`

## Grace Period and Reclamation

- `synchronize_rcu()`: blocks until all pre-existing read-side critical sections complete
- `call_rcu(&head, callback)`: defers callback until after a grace period (non-blocking)
- `kfree_rcu(ptr, rhf)`: shorthand for `call_rcu()` that calls `kfree()` in the callback; `rhf` is the `rcu_head` field name
- `kfree_rcu_mightsleep(ptr)`: head-less variant that requires no `rcu_head` field but must be called from sleepable context

## RCU Variants

| Variant | Read-side API | Sleepable |
|---------|--------------|-----------|
| RCU | `rcu_read_lock()` / `rcu_read_unlock()` | No |
| SRCU | `srcu_read_lock()` / `srcu_read_unlock()` | Yes |
| Tasks RCU | (implicit) | N/A |
| Tasks Trace RCU | `rcu_read_lock_trace()` / `rcu_read_unlock_trace()` | Yes |

Tasks RCU has no explicit read-side lock — any code that does not voluntarily
context-switch is implicitly in a read-side critical section.

## Quick Checks

- `rcu_read_lock_held()` for lockdep debug assertions
- `INIT_RCU_HEAD` has been removed from the kernel entirely
- `rcu_barrier()` waits for all pending `call_rcu()` callbacks to complete (needed at module unload)

## RCU-001: Remove Before Reclaim Ordering

Objects must be removed from RCU-protected data structures before calling
`call_rcu()` or `synchronize_rcu()`. This is because `call_rcu()` only waits
for readers that existed when it was called — it provides no protection against
readers that start after the grace period begins. If the object is still linked
in the data structure, new readers can find it and access it after it is freed.

The correct sequence:

1. Remove from data structure — prevents new readers from finding the object
2. `call_rcu()` or `synchronize_rcu()` — waits for existing readers to finish
3. Free the resource — in the callback or after `synchronize_rcu()` returns

Use the appropriate RCU-aware removal helpers: `hlist_del_rcu()`,
`list_del_rcu()`, `rhashtable_remove_fast()`, etc.

```c
// WRONG — removal after call_rcu causes use-after-free
call_rcu(&obj->rcu, free_callback);

void free_callback(struct rcu_head *rhp) {
    struct obj *obj = container_of(rhp, struct obj, rcu);
    hlist_del_rcu(&obj->node);  // Too late: new readers already found it
    kfree(obj);
}
```

```c
// CORRECT — remove first, then defer freeing
hlist_del_rcu(&obj->node);         // No new readers can find it
call_rcu(&obj->rcu, free_callback);

void free_callback(struct rcu_head *rhp) {
    struct obj *obj = container_of(rhp, struct obj, rcu);
    kfree(obj);                    // Safe: all prior readers are done
}
```

**REPORT as bugs**: Code that calls `call_rcu()` or `kfree_rcu()` on an object
that is still reachable through an RCU-protected data structure, or code that
performs the removal inside the RCU callback rather than before it.
