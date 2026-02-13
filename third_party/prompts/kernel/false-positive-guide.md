# False Positive Prevention Guide

It is critical this prompt is fully processed in a careful, systematic way.
It is used during the false positive section of the review, where avoiding
false positives is of utmost importance.  You need to shift all bias
away from efficient processing and focus on following these instructions
as carefully as possible.

## Core Principle
**If you cannot prove an issue exists with concrete evidence, do not report it.**

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

### 2. API Misuse Assumptions
**Never report** issues based on theoretical API misuse unless you can prove:
  - An actual calling path exists that triggers the issue
  - The function naming/documentation doesn't clearly indicate usage constraints
  - Similar kernel APIs validate the same preconditions

### 3. Unverifiable Assumptions
**Assume the author is wrong** and require proof they are correct
- Look for the author in the MAINTAINERS file, if found, assume their comments,
  commit messages and assertions are correct.
- Untrusted sources (network/user) always need concrete proof of correctness
- Research assumptions and claims in commit messages, comments and code, prove them correct
- If the author makes claims without code evidence, treat them as unverified
- Design decisions must be justified by code or documentation
- Read the entire commit message. If the commit message explains a given behavior,
verify the explanation is correct with code evidence.
- Read the surrounding code comments. Verify comments accurately describe the code behavior.

**Report unless**:
- You found specific code that proves the author correct
- You can verify all assumptions with concrete code paths
- The behavior is proven correct, not just claimed

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
**You're especially bad at finding races, assume you're wrong unless you have concrete proof**
- Identify the EXACT data structure names and definitions
  - Output: struct name and location
- Identify the locks that should protect them
  - Output: lock name and where it's defined
- Prove the race exists with CODE SNIPPETS
  - Output: two code paths that can execute concurrently, with locations

**Just because** operations moved doesn't mean it's wrong.

### 9. Performance Tradeoffs
**Not a regression if**:
- Lower performance was an intentional tradeoff
- Commit message explains the performance impact
- Simplicity/maintainability was prioritized
- It's optimizing for a different use case

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

### 13. Implicit Guard Conditions

**Before reporting NULL dereference**:
- Review technical-patterns.md "NULL Pointer Dereference" section
- Load and fully analyze pointer-guards.md for EVERY NULL pointer

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

## TASK POSITIVE.1 Verification Checklist

Complete each verification step below and produce the required output.
Do not skip steps. Do not claim completion without producing the output.

Before reporting ANY regression, verify:

0. For NULL pointer dereferences, review technical-patterns.md and load pointer-guards.md
   - Output: "reviewed" or "not applicable - not a NULL dereference issue"
1. **Can I prove this path executes?**
   - Find calling code that reaches here
     - Output: quote the call chain with locations (e.g., "caller@file:line → target@file:line")
   - Check for impossible conditions blocking the path
     - Output: list conditions checked and their evaluation
   - Verify not in dead code or disabled features
     - Output: enabled-by config option or "always enabled"
2. **Is the bad behavior guaranteed?**
   - Prove "will happen" not just "might happen"
     - Output: step-by-step execution path with function names and locations showing the failure
   - Prove "causes failure" not just "increases risk"
     - Output: the specific failure mode and triggering condition
3. **Did I check the full context?**
   - Examine calling functions (2-3 levels up)
     - Output: list each caller checked with a random line from each
   - Check initialization and cleanup paths
     - Output: init/cleanup functions examined with locations
   - Verify subsystem conventions
     - Output: conventions found and whether code follows them
4. **Is this actually wrong?**
   - Check if intentional design choice
     - Output: quote commit message or comment if explains intent, else "no explanation found"
   - Check if documented limitation
     - Output: quote documentation if found, else "not documented"
   - Verify not test code allowed to be imperfect
     - Output: "production code" or "test code - severity adjusted"
   - Confirm bug exists today, not just if code changes later
     - Output: current triggering path or "theoretical future issue only"
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
9. **Debate yourself**
   - Do these two steps in order:
   - 9.1 Pretend you are the author. Think extremely hard about the review and try to prove it incorrect.
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
       * Are they being overly defensive about theoretical issues?
     - Output: strongest argument against reporting this bug
   - 9.2 Now pretend you're the reviewer. Think extremely hard about the author's arguments and decide if the review is correct.
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
- Memory leaks in test programs → Usually OK
- File descriptor leaks in tests → Usually OK
- Unless it crashes/hangs the system → Report it

### Assertions and Warnings
- Removing WARN_ON/BUG_ON → Not a regression
- Removing BUILD_BUG_ON → Not a regression
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
2. **Would an expert see this as a real issue?** [ yes / no ]
3. **Is this worth the maintainer's time?** [ yes / no ]
4. **Am I being overly defensive?** [ yes / no ]

### MANDATORY Final Filter validation

If you didn't answer yes to all 4 questions, investigate further or discard

## Remember
- **False positives waste everyone's time**
- **Kernel developers are experts** - respect their judgment
- **Real bugs have real proof**
