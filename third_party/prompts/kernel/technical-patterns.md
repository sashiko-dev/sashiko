# Linux Kernel Technical Deep-dive Patterns

## Core instructions

- Trace full execution flow, gather additional context from the call chain to make sure you fully understand
- IMPORTANT: never make assumptions based on return types, checks, WARN_ON(), BUG_ON(), comments, or error
  handling patterns - explicitly verify the code is correct by tracing concrete execution paths
- IMPORTANT: never assume that changing a WARN_ON() or BUG_ON() statement changes the
  errors or conditions a function can accept.  They indicate changes to
  what is printed to the console, and nothing else.
- IMPORTANT: never skip any steps just because you found a bug in a previous step.
- Never report errors without checking to see if the error is impossible in the
  call path you found.
    - Some call paths might always check IS_ENABLED(feature) before
      dereferencing a variable
    - The actual implementations of "feature" might not have those checks,
      because they are never called unless the feature is on
    - It's not enough for the API contract to be unclear, you must prove the
    bug can happen in practice.
    - Do not recommend defensive programming unless it fixes a proven bug.

### Error Handling

**Notes**:
- If code checks for a condition via WARN_ON() or BUG_ON() assume that condition will never happen, unless you can provide concrete evidence of that condition existing via code snippets and call traces

### Bounds & Validation

**Important**: Never suggest defensive bounds checks unless you can prove the source is untrusted.

### Kernel Context Rules
- **Preemption disabled**: Can use per-cpu vars, may be interrupted by IRQs
- **Migration disabled**: Stay on current CPU but may be preempted
- **typeof() safety**: Can be used with container_of() before init
- **Self-tests**: Memory leaks/FD leaks acceptable unless they can crash the system
- **likely()/unlikely()**: don't report on changes to compiler hinting unless
  they introduce larger logic bugs
- READ_ONCE() is not required when the data structure being read is protected by a lock we're currently holding

### RCU Mandatory Check
- **CRITICAL**: When you see `call_rcu()`, `synchronize_rcu()`, or `kfree_rcu()`:
  - IMMEDIATELY load `subsystem/rcu.md`
  - Check: does removal from any data structure happen BEFORE or AFTER the call_rcu()?
  - If removal is in the RCU callback → this is the WRONG pattern, flag as use-after-free
- The correct order is: **remove from data structure FIRST**, then call_rcu() or synchronize_rcu(), then free in callback
- call_rcu() runs AFTER the rcu grace period is done, allowing all readers to finish,
  a kfree() before the grace period potentially frees while readers are using the pointer.
- Common dangerous pattern to watch for:
  ```
  call_rcu(&obj->rcu, callback);
  // In callback:
  void callback(...) {
      remove_from_structure(obj);  // WRONG: too late!
      kfree(obj);
  }
  ```

### list_head APIs
- list_add(new, head) calls (and others in the list_head API) initialize `new` by writing to it,
  but `head` must have been previously initialized.
- when objects are removed from lists, make sure they are returned, freed, or not otherwise lost

### Resource Management Knowledge
- Every resource must have balanced lifecycle: alloc→init→use→cleanup→free
- All pointers have the same size: `char *foo` takes as much room as `int *foo`
  - but for code clarity, if we're allocating an array of pointers, and using
    `sizeof(type *)` to calculate the size, we should use the correct type
- refcount_t counters do not get incremented after dropping to zero
- refcount_dec_and_test returns true only at zero
- css_get() adds an additional reference, ex: this results in both sk and newsk having one reference each:
```
     memcg = mem_cgroup_from_sk(sk);
     if (memcg)
             css_get(&memcg->css);
     newsk->sk_memcg = sk->sk_memcg;
```
- If you find a type mismatch (using `*foo` instead of `foo` etc), trace the type
  fully and check against the expected type to make sure you're flagging it correctly
- global variables and static variables are zero filled automatically
- slab and vmalloc APIs have variants that zero fill, and __GFP_ZERO gfp mask does as well
- kmem_cache_create() can use an init_once() function to initialize slab objects
  - this only happens on the first allocation, and protects us from garbage in the struct
- when freeing/destroying resources referenced by structure fields, ensure pointer fields are set to NULL to prevent use-after-free on reuse if the structure is not also immediately freed
  - ex: unregister_foo() { foo->dead = 1; free(foo->ptr); add to list}
       register_foo() { pull from list ; skip allocation of foo->ptr; foo->ptr->use_after_free;}
  - safe: kfree(foo->ptr); ... ; kfree(foo);
    - nobody will find foo->ptr is non-NULL because foo is gone
  - Assume [kv]free(); [kv]malloc(); APIs handle this properly unless you find proof initialization is skipped

### for loops
- for(init; condition; advance) { body } -- checks 'condition' BEFORE executing 'body'
- for(init; condition; advance) { body } -- 'advance' only runs AFTER 'body'

### Additional resource checks
- Resource switching detection:
  - Check every path where a function returns a different resource than it was meant to modify
  - ensure the proper locks are held or released as needed.
- Caller expectation tracing: What does the caller expect to happen to the resources it passed into functions?

- strscpy() auto-detects array sizes when compiler can find the type
- `char *s = ""`; `strlen(s)` returns zero, but `s[0]` is safe to access
- memcpy(dst + (offset & mask), src, size) usually has alignment validation elsewhere
- Global arrays with MAX_FOO: check if it is possible to create more than MAX_FOO elements

### ERR_PTR vs NULL
- ERR_PTR() holds an error cast to a pointer, but they are not valid pointers
  - foo = ERR_PTR(-ENOMEM) ; if (foo) -> TRUE, but *foo will CRASH

### NULL Pointer Dereference

Review the examples carefully before analyzing NULL pointer dereferences.
Misunderstanding what constitutes a dereference causes false positives.

**Dereference Types**:
```
- val = *foo dereferences foo
  - if (foo) val = *foo is safe
- val = foo[var] dereferences foo but only reads var without dereference
  - if (foo) val = foo[var] is safe
- val = foo->ptr dereferences foo but only reads ptr without dereference
  - if (foo) val = foo->ptr is safe
- val = *foo->ptr dereferences foo and ptr.
  - if (foo && foo->ptr) val = *foo->ptr is safe
- val = (*foo)->ptr dereferences foo, then dereferences what foo points to, then
         reads ptr without dereference
  - if (foo && *foo) val = (*foo)->ptr is safe
- val = foo->ptr->something dereferences foo and ptr but only reads something
  - if (foo && foo->ptr) val = foo->ptr->something is safe
```

**Key Points**:
1. **Reading a pointer field is not the same as dereferencing it**
   - `ptr = foo->bar` dereferences `foo`, reads `bar`, but does NOT dereference `bar`
   - The dereference happens when you later USE `ptr`

2. **Check where the pointer is actually used**
   - If `ptr = foo->bar` and `bar` can be NULL, the problem occurs when `ptr` is dereferenced
   - Example: `ptr->field` or `*ptr` or `ptr[0]` or passing `ptr` to a function that dereferences it

3. **NULL checks protect the pointer being checked**
   - `if (foo)` protects dereferencing `foo`
   - `if (foo && foo->bar)` protects dereferencing both `foo` and `foo->bar`
