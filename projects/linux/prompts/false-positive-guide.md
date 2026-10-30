# False Positive Prevention Guide

It is critical this prompt is fully processed in a careful, systematic way.
It is used during the false positive section of the review, where avoiding
false positives is of utmost importance.  You need to shift all bias
away from efficient processing and focus on following these instructions
as carefully as possible.

## Core Principle
**If you cannot prove an issue exists with concrete evidence in at least one execution path, do not report it.**

**Corollary (from callstack.md)**: For deadlocks, infinite waits, crashes, and
data corruption, "concrete evidence" means proving the code path is structurally
possible — not proving it will definitely execute on every run. A
`wait_event` with no timeout and no fallback wake condition is a deadlock bug
if the wake condition depends on external events that can stop. Do not dismiss
such bugs as "unlikely in practice."

This file contains instructions to help you prove a given bug is real.  You
must follow every instruction in every section.  Do not skip steps, and you
must complete task POSITIVE.1 before completing the false positive check.

## Common False Positive Patterns

### 0. Context preservation
- If you're analyzing a git commit make sure the full commit message is still in context.  If not, reload it.
- If you're processing a patch instead of a commit, make sure the full
  patch description is still in context.  If not, reread it.
- Confirm this context is available for the false positive section
- Do not proceed with false positive verification without this context ready

### 1. Defensive Programming Requests
**Never suggest** defensive checks unless you can prove:
- The input comes from an untrusted source (ex: user/network)
- An actual path exists where invalid data reaches the code
- The current code can demonstrably fail

**Examples**:
- ❌ "Add bounds check here for safety"
- ❌ "This should validate the index"
- ✅ "User input at funcA() can reach this without validation"

### 1.1 Failure to handle errors
**Never report** failure to handle errors unless
  - You can prove the error is possible
  - You've confirmed the function arguments used don't prevent the error

### 1.2. Mandatory API Contract Violations vs. Defensive Requests
Do not confuse **mandatory kernel API error-handling** with "defensive programming requests."
* If newly added code calls an allocation, initialization, locking, or resource management function (e.g., `kmalloc`, `devm_pm_runtime_enable`, `pm_runtime_get_sync`, `mutex_lock_interruptible`, `device_add_software_node`), checking and propagating its return value is an **absolute API contract requirement**.
* Failing to check these returns is a **concrete bug**, not a defensive suggestion, and **must be reported**.
* Do not discard ignored return values under the assumption that they are "low probability" (e.g., "memory allocation rarely fails during probe" or "PLL lock timeout is unlikely"). In the Linux kernel, all resource acquisition errors must be handled robustly.

* **EXCEPTIONS to Mandatory Error Handling:**
  1. **Early Boot / Init Code:** Handling allocation or initialization failures is not required in early boot initialization code (typically functions annotated with `__init` or executing during early architecture setup) because a failure here is catastrophic and recovery is impossible.
  2. **Optional / Debug APIs (DebugFS):** Do not flag missing checks on opportunistic debug APIs (specifically `debugfs_create_file`, `debugfs_create_dir`, and other debugfs functions), as kernel policy explicitly discourages checking them.
  3. **Cleanup & Teardown Paths:** During error rollback or driver removal (e.g., `remove` callbacks), checking the return values of release/disable calls (e.g., `clk_disable_unprepare`, `pm_runtime_disable`) is not required, as teardown must proceed regardless of individual release failures.
  4. **Explicitly Documented Omissions:** Do not report ignored returns if the call is explicitly cast to `(void)` or accompanied by a code comment explaining why the omission is safe, unless you can prove their specific reasoning is factually flawed.

### 2. API Misuse Assumptions
**Never report** issues based on theoretical API misuse unless you can prove:
  - An actual calling path exists in the patch series or kernel tree that triggers the issue
  - The function naming/documentation doesn't clearly indicate usage constraints
  - Similar kernel APIs validate the same preconditions
- **No speculative driver/caller footguns**: Do not report that a new or refactored internal helper could fail if a hypothetical or out-of-tree caller passes an invalid argument (e.g., a stack-allocated array where a persistent array is required, a tail page where an order-0 page or head page is required, `nr_pages = 0` or an overflowing `nr_pages`, or a current-EL syndrome to a lower-EL trap helper) unless a caller in the patch series actually violates the contract.
- **Internal API contract `WARN_ON_ONCE` / `VM_WARN_ON_ONCE`**: Do not report `WARN_ON_ONCE()` or `VM_WARN_ON_ONCE()` checks that enforce kernel driver API contracts as user-triggerable `panic_on_warn` Denial-of-Service bugs unless you can prove an actual in-tree driver violates the contract on a user-reachable path.

### 3. Unverifiable Assumptions
**Assume the author is wrong** and require proof they are correct
- Untrusted sources (network/user) always need concrete proof of correctness
- Research assumptions and claims in commit messages, comments and code, prove whether they are correct
- If the author makes claims without code evidence, treat them as unverified
- Design decisions must be justified by code or documentation
- Read the entire commit message. If the commit message explains a given behavior,
verify the explanation is correct with code evidence.
- Read the surrounding code comments. Verify comments accurately describe the code behavior.

**Report unless**:
- You found specific code that proves the author correct
- You can verify all assumptions with concrete code paths
- The behavior is proven correct, not just claimed

### 3.1 Comment-Based Dismissals (MANDATORY)
**CRITICAL**: When dismissing an issue because a comment or documentation says
the code behaves a certain way, you MUST verify against the actual implementation:

1. **Read the function body, not just the comment**
   - Comments can be copy-pasted to multiple implementations with different semantics
   - The same comment may appear on both sides of an `#ifdef/#else` block
   - Output: quote the actual implementation code, not just the comment

2. **Check for conditional compilation**
   - If code has `#ifdef CONFIG_FOO` / `#else` branches, determine which applies
   - A comment describing behavior in one branch may not apply to the other
   - Output: which config branch applies and why

3. **Verify helper function behavior**
   - If dismissing because "function X returns Y", read function X's implementation
   - Check if function X has config-dependent behavior
   - Output: quote function X's implementation showing it guarantees the claimed behavior

4. **When in doubt, report the issue**
    - If you cannot verify the comment matches the implementation under all configs, report it
    - A bug dismissed based on incorrect documentation is worse than a false positive

### 3.2. Subsystem & Hardware Invariants (MANDATORY)
**Before flagging missing input/bounds validation or error-pointer checks, verify if the subsystem's invariants render the issue impossible:**
- **Subsystem Pre-Validation Engine & Tooling Guarantees:** Does the subsystem or the language/compiler tooling ecosystem (e.g., BPF's verifier, Rust's compiler safety constraints, Sparse annotations, lockdep, or device tree schemas) statically or dynamically guarantee that the safety invariant is enforced? If so, do not suggest redundant manual checks. However, when dismissing an out-of-bounds or parsing bug based on an upstream helper or validator (such as `*_size_ok` or a pre-decode check), you MUST read that validator's implementation and verify that every flag or branch condition (such as default-mapping flags or post-decode empty outputs) guarantees the consumer's access.
- **Hardware vs. Software Array Bounds:** Never dismiss a missing bounds check on a fixed-capacity kernel software array or fragment list (such as `skb_shinfo(skb)->frags[MAX_SKB_FRAGS]` in `skb_add_rx_frag()`) inside a descriptor or packet loop by assuming hardware initialization registers or MTU settings cap the iteration count.
- **Architecture & Context Constraints:** Is the flagged issue (e.g., unaligned memory access or 32-bit register layout corruption) only physically possible on hardware architectures that are irrelevant to or unsupported by the target subsystem or driver?
- **Intentional Error Pointers:** Do not flag callee functions for returning standard kernel error pointers (e.g., `ERR_PTR(-ENOENT)` or `ERR_PTR(-ENOMEM)`) instead of NULL. Returning an `ERR_PTR` is a standard, deliberate design pattern in the Linux kernel to propagate specific failure codes. (However, always verify that the *callers* of such functions properly validate the returned value using `IS_ERR()` or `IS_ERR_OR_NULL()`).
- **Hardware Quirks:** Is the flagged sequence (e.g., writing to registers in a seemingly "wrong" order or after sleep) explicitly required by the hardware specification?

*Output Verification:*
- Subsystem invariants checked: [ list invariants or "none" ]
- Verifier/compiler guarantees found: [ list guarantees or "none" ]
- Rationale for discarding/reporting: [ write brief explanation ]

### 4. Locking False Positives
**Before reporting** a locking issue:
- Check ALL calling functions for held locks
  - Output: list each caller and locks it holds (e.g., "caller() holds mutex_x at file:line")
- Trace up 2-3 levels to find lock context
  - Output: full lock chain from entry point to issue site
- Verify the actual lock requirements
  - Output: quote lock documentation or convention (e.g., "must hold rcu_read_lock")
- Consider RCU and other lockless mechanisms
  - Output: RCU/lockless mechanism found or "none applicable"

**Common mistakes**:
- Missing that caller holds the required lock
- Not recognizing RCU-protected sections
- Assuming all shared data needs traditional locks

### 4.1. Lifecycle Concurrency & Serialization
**Do not report concurrency issues (data races, locking violations, Use-After-Free) in serialized lifecycle functions:**
- **Initialization Phase:** Functions like `probe()`, `setup()`, `init()`, and early boot initialization. During these phases, the device is not yet registered or visible to userspace/network/client drivers, meaning concurrent access is structurally impossible.
- **Teardown Phase:** Functions like `shutdown()`, `remove()`, or final cleanup callbacks. These are typically serialized by the driver core, and parent subsystems (like the PCI core) have already disabled DMA and interrupts.
- **Parent Subsystem/Bus Protection:** Check if the parent bus core (e.g., PCI, USB, platform bus, or driver core) automatically handles resource teardown, state resets, or cleanup (such as disabling DMA, clearing bus mastering, or freeing devm-managed memory) rendering explicit local driver cleanup redundant.

*Output Verification:*
- Function execution phase: [ e.g., probe, normal runtime, shutdown ]
- Proof of concurrent access path: [ quote concurrent thread spawn, or write "serialized lifecycle - no race possible" ]

### 5. Use-After-Free Confusion
**Distinguish between**:
- Use-after-free (accessing freed memory) ← Report this
- Use-before-free (using then freeing) ← Don't report
- Free-after-use (normal cleanup) ← Don't report

**Verification**:
- Trace the exact sequence of operations
  - Output: sequence showing "alloc@loc → use@loc → free@loc → use@loc" or "no UAF found"
- Check if object ownership was transferred
  - Output: ownership transfer point or "ownership retained"

### 6. Resource Leak Misconceptions
**Not a leak if**:
- Ownership was transferred to another subsystem
- Object was added to a list/queue for later processing
- Cleanup happens in a callback or delayed work
- It's in test code and doesn't affect the system
- It's in a short-lived userspace CLI build utility (`scripts/`, `tools/objtool/`) on a fatal error path or at process exit (e.g., `ptr = realloc(ptr, size)` losing the old pointer on OOM right before exiting, early error returns skipping `free()`, or `ferror(f) || fclose(f)` skipping `fclose(f)` before process termination), where the OS immediately reclaims all memory and file descriptors

**Verify by**:
- Trace object ownership changes
  - Output: ownership chain "alloc@loc → stored in X@loc → freed by Y@loc" or "leak confirmed"
- Check for async cleanup mechanisms
  - Output: cleanup callback or workqueue handler, or "no async cleanup found"
- Understand subsystem ownership models
  - Output: quote subsystem convention or "no documented model"

### 7. Order Changes
**Don't report** order changes unless you can prove:
- A race condition is introduced
- A dependency is violated
- An ABBA deadlock pattern emerges
- State becomes invalid

### 8. Races
- Identify the EXACT data structure names and definitions
  - Output: struct name and location
- Identify the locks that should protect them
  - Output: lock name and where it's defined
- Prove the race exists with CODE SNIPPETS
  - Output: two code paths that can execute concurrently, with locations

### 8.1. Race Dismissal: Full-Path Verification (MANDATORY)
When dismissing a race because "the code detects the invalid state and aborts,"
you MUST verify the ENTIRE instruction sequence between the race window and the
recovery point. A single abort path later in the function does not make earlier
dereferences safe.

Before accepting a race dismissal, answer ALL of these:
1. What exact instruction opens the race window?
   - Output: function, file:line, what state becomes stale
2. What exact instruction closes it (drain/barrier/lock)?
   - Output: function, file:line, synchronization mechanism
3. What is the "graceful handler" you claim makes this safe?
   - Output: function, file:line, how it detects invalid state
4. **List every instruction between #1 and #3 that touches the contested
   resource. Are ALL of them safe if the resource was invalidated by the
   racing thread?**
   - Output: enumerate each instruction with verdict (safe/unsafe)

If you cannot affirmatively answer #4 for every intermediate instruction,
the dismissal is invalid. Report the race.

### 8.2. Race reference budget (MANDATORY)
For any suspected use-after-free where the other side is asynchronous work, an
RCU callback or a workqueue item, apply "Step 5: Check Object Lifetime" in
`locking.md`, loading it if it is not already loaded.

Required output: the `budget:` line defined there. If analysis already
produced one, check it against the code again rather than copying it.

### 9. Performance Tradeoffs
**Not a regression if**:
- Lower performance was an intentional tradeoff explicitly documented in the commit message or code comments
- Commit message explains the performance impact
- Simplicity/maintainability was explicitly prioritized by the author in the commit message or comments (never invent an unstated "simplicity tradeoff" to excuse operating on unmodified ranges/entries when the code tracks modified sub-ranges, or to excuse narrowing an existing Kconfig/UAPI feature)
- It's optimizing for a different use case

### 9.1 Documented API Guarantees
**Never report** that an API fails to provide something its own documentation
and kernel-doc promise, unless you can quote the implementation defeating the
promise. A documented guarantee binds every configuration, including debug
ones. Do not speculate that `SLUB_DEBUG`, `KASAN`, `kmemleak`, redzones,
padding or instrumentation quietly opt out of it.

Callers legitimately rely on documented guarantees without re-checking them.
Finding such a caller is not evidence that the guarantee is violated, and the
absence of a defensive check at the call site is not a bug.

**Most common instance -- kmalloc alignment**:
`kmalloc()`/`kzalloc()` of a power-of-two size is aligned to at least that
size, so `kmalloc(PAGE_SIZE, ...)` is always page-aligned. This is stated in
`Documentation/core-api/memory-allocation.rst` and the `kmalloc()` kernel-doc
in `include/linux/slab.h`, and enforced in `create_boot_cache()`
(`mm/slab_common.c`) and `calculate_sizes()` (`mm/slub.c`), which rounds
redzone padding up to the cache alignment.

- ❌ "This buffer may not be page-aligned, because SLUB debugging can add
  redzones, so the `BUG_ON(!PAGE_ALIGNED(buf))` in a driver using it can fire."
- ✅ "The size is `UART_XMIT_SIZE * 3`, which is not a power of two, so the
  allocation is only guaranteed `UART_XMIT_SIZE` alignment, while `foo()`
  masks the low `PAGE_SHIFT` bits of the address."

Check the allocator code (and its code notes) for the full alignment
rules and for the constraints on kmalloc memory that are real (`struct
page` requirements, silently-ignored `__GFP_DMA32`).

### 9.2. Practical Severity vs. Theoretical Pathology
**Do not waste developer time with theoretical, non-impactful bugs:**
- **Complexity Trade-offs:** Do not flag O(N^2) or high-complexity algorithms in helper, test, or setup code if they are chosen for simplicity and cannot be abused by untrusted input to cause a DoS.
- **Practically Bounded Values:** Do not flag potential overflows on variables that are practically bounded (e.g., NUMA node IDs, CPUs, or hardware channels that will never exceed limits in real-world systems).
- **Security Privilege Boundary Equivalence:** Do not flag minor information leaks if the receiving context already possesses equivalent or higher privileges that allow reading/writing that memory anyway (e.g., leaking a kernel memory address or stack structure to a context that already has native kernel-level read privileges).

*Output Verification:*
- Practical impact of worst-case: [ e.g., "negligible", "DoS risk", "no impact" ]
- Security boundary violated: [ e.g., "none - context has read_kernel privilege", "yes - user-space leak" ]

### 10. Intentional backwards compatibility
- Leaving stub sysfs or procfs files is not required, and also not a regression
- It is not a regression for deprecated sysfs files to remain and just return
  any constant value (0, empty strings, a specific fixed string are all ok),
  as long as that value was legal for the interface before deprecation.

**ONLY REPORT**: if you can prove the resource contract has been broken

### 11. Subjective review patterns
- problems flagged by SR-* patterns are not bugs, they are opinions.
- But, they can still be wrong.  Focus on checking against the commit message,
nearby code, nearby comments, and the "debate yourself" section of the
verification checklist.

### 12. Uninitialized variables
- assigning to a variable is the same as initializing it.
- passing uninitialized variables to a function is fine if that function writes
to them before reading them
- only report reading from uninitialized variables, not writing to them.
- `kzalloc`/`kcalloc` zero all bytes. Do not flag missing explicit
  initialization for fields whose zero value is correct (NULL pointers,
  zero counts, empty list heads where `*_INIT` is `{NULL}` or `{0}`).

### 13. Implicit Guard Conditions

**Before reporting NULL dereference**:
- Trace every condition on the path to the dereference (loop filters,
  early returns, helper checks) and show that none of them rules out
  NULL, including guards on other state that is set together with the
  pointer

### 14. Patch series false positive removal

Large changes are broken up into small logical units in order to make them
easier to understand and review.

- Example correct patch series:
  - PATCH 1: add a new API
  - PATCH 2: change one subsystem or one file to use the new API
  - PATCH 3-N: change all the other subsystems or files to use new API
  - PATCH N+1: delete the old API

Do not try to review the judgements made in breaking up large changes.  Just
look for objective bugs as per the review prompts and false positive guide.

If our potential bug is simply work in progress that is completed later in the series,
it is a false positive and should be ignored.

- Example incorrect patch series:
  - PATCH 1: create a regression (crash, overflow, various bugs)
  - PATCH 2: fix that regression

We expect each patch in the series to be working toward a larger goal, BUT
we require each patch to be self contained and correct.  Specifically:

- Each patch must compile
- New bugs must not be introduced

Intermediate patches in a series may intentionally introduce performance issues
that are fixed later in the series.  The commit message or comments in the code
should explain how this was intentional.

If you've identified a real regression fixed later in the patch series, you
must still report this regression []
  - BUT, you must indicate in the bug report that you found the fix later in
    the series []
  - When reporting, include both the commit sha and the commit subject line []

#### Patch series Mandatory Validation
- Was a git range provided in the prompt? [ y / n, range ]
- Did you use it to search forward? [ y / n ]

### 15. Subsystem guide violations (hallucination check ONLY)

Issues tagged `subsystem_guide_violation: true`, or with category
`guide-directive` or `issue_type: "potential-issue"` with a `guide_directive`
field, are subsystem guide violations. The subsystem guide is authoritative —
the violation itself is treated as factually correct.

**STOP. For these issues, ONLY perform these three hallucination checks. Do
NOTHING else. Do NOT apply sections 1-14. Do NOT apply TASK POSITIVE.1.**

1. **Does the cited guide rule exist?** Re-read the subsystem guide and confirm
   the quoted directive actually appears in the guide text. If the agent
   fabricated or misquoted the rule, eliminate the issue.

2. **Does the cited code exist?** Confirm the function, variable, or code
   pattern the agent cited is real. Use `find_function` or read the file. If
   the agent hallucinated the code (wrong function name, nonexistent variable,
   fabricated code path), eliminate the issue.

3. **Does the code actually violate the guide rule?** Read the cited code and
   the guide rule side by side. Confirm the code does the thing the guide says
   not to do (or fails to do the thing the guide requires). If the agent
   mismatched rule to code — e.g., the guide prohibits pattern X but the code
   does pattern Y, or the guide requires lock L but the code already holds
   lock L — eliminate the issue.

**If all three checks pass, PRESERVE the issue. You are done.**

**Explicit prohibitions for subsystem guide violations:**
- Do NOT analyze whether the bug is "real" or "theoretical"
- Do NOT check if the code "handles it gracefully"
- Do NOT evaluate locking, races, reachability, or safety
- Do NOT apply your own reasoning about whether the pattern is dangerous
- Do NOT check callers, callees, or context beyond confirming the code exists
- Do NOT debate yourself about the issue
- The ONLY reason to eliminate is hallucination: fabricated rule or fabricated code

## TASK POSITIVE.1 Verification Checklist

Complete each verification step below and produce the required output.
Do not skip steps. Do not claim completion without producing the output.

Before reporting ANY regression, verify:

0. For NULL pointer dereferences, check the guards on the path (section 13)
   - Output: each guard and why it does not rule out NULL, or "not
     applicable - not a NULL dereference issue"
1. **Can I prove this path executes?**
   - Find calling code that reaches here
     - Output: quote the call chain with locations (e.g., "caller@file:line → target@file:line")
   - Check for impossible conditions blocking the path
     - Output: list conditions checked and their evaluation
   - Verify not in dead code or disabled features
     - Output: enabled-by config option or "always enabled"
2. **Is the bad behavior structurally possible?**
   - Prove the code path exists and the triggering conditions are not structurally impossible
     - Output: step-by-step execution path with function names and locations showing the failure
   - Prove the failure mode is concrete (crash, deadlock, corruption, leak), not just "increases risk"
     - Output: the specific failure mode and triggering condition
   - NOTE: A deadlock, infinite wait, or crash that depends on runtime conditions
     (timing, memory pressure, shutdown state, allocation patterns) is a real bug
     if the code has no structural prevention (timeout, fallback wake condition,
     bounded retry). Do not dismiss these by arguing the conditions are unlikely.
3. **Did I check the full context?**
   - Examine calling functions (2-3 levels up)
     - Output: list each caller checked with a random line from each
   - Check initialization and cleanup paths
     - Output: init/cleanup functions examined with locations
   - Verify subsystem conventions
     - Output: conventions found and whether code follows them
4. **Is this actually wrong?**
   - Check if intentional design choice
     - Output: quote commit message or comment if indicates that author thought of the same issue, else "no explanation found"
   - Check if documented limitation
     - Output: quote documentation if found, else "not documented"
   - Verify not test code allowed to be imperfect
     - Output: "production code" or "test code - severity adjusted"
   - Confirm bug exists today, not just if code changes later
     - Output: current triggering path or "theoretical future issue only"
     - NOTE: "theoretical" means the code path cannot be reached today.
       A bug that depends on runtime conditions (timing, system state) is
       not theoretical — it is a real bug with a conditional trigger.
5. **Did I check the commit message and surrounding comments?**
   - Read the entire commit message
     - Output: quote any text explaining this behavior, or "no explanation found"
   - Read surrounding code comments
     - Output: quote relevant comments, or "no relevant comments"
6. **When complex multi-step conditions are required for the bug to exist**
   - Prove these conditions are actually possible
     - Output: code path showing each condition can be true simultaneously
7. **Did I hallucinate a problem that doesn't actually exist?**
   - Verify the bug report matches the actual code
     - Output: quote the exact code snippet from the file
   - Reread the file and confirm code matches your analysis
     - Output: file:line and verbatim code
   - Check your math (division by zero requires zero in denominator, etc.)
     - Output: arithmetic verification or "no arithmetic involved"
8. **Did I check for future fixes in the same patch series?**
   - Search forward in git history (not back), only on this branch
     - Output: commits checked or "no git range provided"
   - If fix found later in series
     - Output: "found fix in [commit] - reporting as real bug with later fix" or "no fix found"
9. **If dismissing based on comments or documentation, verify the implementation**
   - Did you read the actual function implementation, not just the comment?
     - Output: quote the implementation code that proves the comment is accurate
   - Does the function have #ifdef/#else branches with different behavior?
     - Output: list config options that affect behavior, state which applies
   - Did you verify helper functions behave as their comments claim?
     - Output: quote helper implementation or "no helper functions involved"
   - If you cannot verify implementation matches documentation, do NOT dismiss
     - Output: "implementation verified" or "cannot verify - reporting issue"

10. **Debate yourself**
   - Do these two steps in order:
   **CRITICAL**: 10.1 generates devil's advocate counterarguments. These are
   HYPOTHESES to investigate, not findings. A counterargument only invalidates
   a bug if 10.2 finds concrete code to back it up.

   - 10.1 Pretend you are the author and generate counterarguments:
     - Check for hallucinations or invented information
     - **For NULL safety, ask as the author:**
       * Did reviewer search for similar code in my subsystem accessing this pointer?
       * If reporting missing NULL check, did they explain why OTHER code in my subsystem HAS that check?
       * Did they verify lifecycle dependencies or just analyze syntactically?
       * Is there semantic coupling between guard condition and pointer validity they missed?
       * Would adding their suggested check be redundant/paranoid given the invariants?
       * Did they compare guard patterns - why does path A check NULL but path B doesn't?
     - **For locking, ask as the author:**
       * Did reviewer check what locks my caller holds?
       * Did they understand lock context (process/softirq/hardirq/RCU)?
       * Is there a lock held higher in the call chain they missed?
     - **For resource leaks, ask as the author:**
       * Did reviewer trace ownership transfer?
       * Did they check for async cleanup mechanisms?
       * Is the resource stored somewhere for later cleanup?
     - **For all issues, ask as the author:**
       * Did they check if this is intentional based on commit message or comments?
       * Did they verify the conditions for the bug are actually possible?
       * Are they confusing a structurally possible bug with a defensive programming suggestion?
     - Output: strongest argument against reporting this bug
     - IMPORTANT: "unlikely in practice" is not a valid argument against a
       deadlock, crash, or data corruption. Only "structurally impossible"
       (the code literally cannot reach that state) is valid. See
       callstack.md "Reachability Dismissals".
   - 10.2 Now pretend you're the reviewer. Think extremely hard about the author's arguments and decide if the review is correct.
     - Address each author argument with code evidence
     - Output: code evidence refuting the author, or "cannot refute with code - likely false positive"

### Mandatory Validation

- If any Output requirement above is blank or skipped, repeat that step
- If you cannot produce code evidence for your conclusion, the bug is likely a false positive

## Patch series
- You may only use this exact method to look forward in git history.
- NEVER invent other methods to look forward in git history.
- If the prompt included a range of git commits to check, look forward
  through that range for later patches that might resolve the bug you found.
- Never search backwards in commit history.

## Special Cases

### Test Code
- Memory leaks in one-shot test programs at process exit → Usually OK
- File descriptor leaks in one-shot test programs at process exit → Usually OK (do report unclosed descriptors/sockets inside per-case helpers or loops that run multiple test cases in one process)
- Unless it crashes/hangs the system or breaks test execution → Report it

### Host Build Utilities (`scripts/`, `tools/objtool/`)
- Memory leaks on fatal `exit()`/`die()` paths or at process exit in one-shot host build generators → Do NOT report (process exits immediately and OS reclaims memory)
- File descriptor / `FILE *` leaks on fatal exit paths (`ferror(f) || fclose(f)` before exit) in one-shot host build generators → Do NOT report
- Do NOT apply this exception to runtime, tracing, profiling, or debugging utilities and libraries under `tools/` (such as `tools/tracing/`, `tools/perf/`, `tools/sched_ext/`, `tools/lib/`) or to non-fatal error-return paths that return control to a caller (such as `ptr = realloc(ptr, ...)` leaking the original buffer when returning an error) → Report those as bugs
- Non-POSIX portability assumptions (`fopen(..., "w")` vs `"wb"`, `struct stat` `.st_mtim` vs `st_mtimespec`, standard coreutils like `seq`) → Do NOT report
- Concurrent worker threads writing the same constant (`true`) to a plain `bool` status flag → Do NOT report

### Assertions and Warnings
- Removing WARN_ON/BUG_ON → Not a regression
- Removing BUILD_BUG_ON → Not a regression
- Adding `WARN_ON_ONCE` / `VM_WARN_ON_ONCE` for internal kernel/driver API contract checks → Not a user-triggerable `panic_on_warn` DoS unless an actual in-tree caller allows user input to violate the contract
- Unless removing critical runtime checks → Then report

### Reverts
- When reviewing reverts, focus on new issues
- Assume the original bug is known/handled
- Don't re-report the original problem

### Subsystem Exclusions
- fs/bcachefs → Skip all issues
- Staging drivers → Lower standards apply
- Example/test code → Focus on system impact only

## Final Filter

Before adding to report, think about the regression and ask:
1. **Do I have proof, not just suspicion?** [ yes / no ]
  - Code snippets showing all components required to trigger the bug count as proof
    - ONLY if the conditions are also proven to be possible
  - Existing defensive pattern checks for the same condition also count as proof.
    - ONLY if you can prove the condition can occur
  - Existing WARN_ON()/BUG_ON() don't count as proof.
  - For deadlocks/hangs: showing that a wait has no timeout and no alternative
    wake condition IS proof. You do not also need to prove the system will
    definitely enter that state.
2. **Would an expert see this as a real issue?** [ yes / no ]
3. **Is this worth the maintainer's time?** [ yes / no ]
4. **Am I suggesting defensive programming, or reporting a concrete bug?** [ yes / no ]
  - Defensive programming: "add a NULL check here for safety" → discard
  - Concrete bug: "this wait_event has no timeout and no fallback wake" → report

### MANDATORY Final Filter validation

If you didn't answer yes to all 4 questions, investigate further or discard

## Remember
- **Reports without a clear proof waste everyone's time**
- **Missed bugs also waste everyone's time** - a deadlock in production is worse than a false positive in review
- **Real bugs have real proof** - but proof means showing the code path exists and has no structural prevention, not proving the bug will definitely fire on every execution
