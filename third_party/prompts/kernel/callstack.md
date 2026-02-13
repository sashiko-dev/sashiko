# Callstack Regression Analysis

This pattern analyzes functions for regressions within the callstack as a whole.
Changes to a function can introduce bugs not only in the function itself, but in
any function it calls (callees) and any function that calls it (callers). This
analysis traverses the full callstack — both up and down — to identify
side effects, unintended consequences, and regressions that would be invisible
when examining modified functions in isolation.

See technical-patterns.md "NULL Pointer Dereference" section for guidance.

Note: foo->ptr dereferences foo BUT NOT ptr

This callstack analysis is required for all non-trivial changes.

---

## CRITICAL: Retraction Rule

If during analysis you conclude something IS a bug and later reverse that
conclusion, you must treat the reversal with extreme skepticism. State the
retraction explicitly, re-examine your dismissal reasoning for logical errors,
and apply a higher burden of proof. "Caller should prevent this" or "normally
handled" are not sufficient — you must prove the triggering condition is
structurally impossible with concrete code references.

## CRITICAL: Reachability Dismissals

A code path that can infinite loop, deadlock, crash, or corrupt data is a bug
even if you believe preconditions make it unlikely. Do not dismiss such bugs
by arguing:
- "The caller normally prevents this input"
- "This only happens if [upstream function] fails"
- "The old code had a worse bug in the same path"
- "Extremely unlikely in practice"

Only dismiss if the triggering condition is **structurally impossible** —
meaning the code literally cannot reach that state regardless of timing,
memory pressure, or concurrent operations.

---

## CRITICAL: Batch All Semcode Calls

**Each API turn re-sends conversation history. Batch all lookups.**

```
❌ find_function(A) → wait → find_function(B) → wait → find_function(C)
✅ find_function(A) + find_function(B) + find_function(C) in ONE message

❌ find_callers(A) → wait → find_callers(B)
✅ find_callers(A) + find_callers(B) in ONE message
```

Before starting Tasks 1-2:
1. Identify ALL callees you need to load
2. Identify ALL callers you need to load
3. Call find_function for ALL of them in ONE message
4. Call find_callers for ALL of them in ONE message

---

Regression analysis requires understanding both the changes made and all of
the ways those changes impact unmodified code.  This part of the prompt gathers
additional context beyond just the modified functions so that we can search
for side effects and unintended consequences that lead to regressions.

Your analysis must search for those side effects, both in modified functions
and up and down the call stack into unmodified code.

# Task 0: Category iteration

CHANGE CATEGORIES should have been loaded into context before calling
callstack.md. Iterate through all of them if they were provided. Otherwise,
consider the entire unit being analyzed a single change category.

It's possible function/type definitions, callers and callees are already loaded
into context as well. There's no need to reload any work already completed, but
if new definitions, callers, or callees are required during analysis, you made
load them.

- For EVERY category identified
- Perform Tasks 1-6 separately
- You must fully complete every category
- Output: Category N of M: name, description
  - Output: callers loaded: [ list, with random lines from each function ]
  - Output: callees loaded: [ list, with random lines from each function ]
- Completion of this prompt requires enumeration and completion for every category

## Task 1: **Callee traversal process**:

**BATCH ALL find_calls AND find_function CALLS IN ONE MESSAGE**

- Step callee.1: Identify all direct callees in modified functions
  - We gather the callees because even small changes in functions can
    cause bugs in the functions they call.  The only way to know is to
    actually read the functions in the callstack.
  - Use semcode `find_calls` (not "find_callees") on each modified function
    to get the complete callee list.  Batch these calls together.
  - Record both the callees and the arguments used
  - **List ALL callees first before loading any**
  - Output: names of callees
- Step callee.2: For each callee, load entire function definition
  - **Call find_function for ALL callees in ONE parallel message**
  - Output: The callee function names, and a random line from anywhere in each definition
    - you must prove you read the callee
- Step callee.3: Trace 2-3 levels deep as needed
  - **Batch additional find_function calls together**
  - Again, small changes higher up in the stack can introduce bugs lower
    down.  You cannot analyze code effectively without looking at the call stack,
    even for changes that you think you understand.
- Step callee.4: Apply all checks below to each callee in the chain
  - Output: The callee function names, and a random line from anywhere in each definition
    - you must prove you read the callee
- Step callee.5: completing the callee analysis is not sufficient.  You must also
complete caller analysis.

## Task 2: **Caller traversal process:**

**BATCH ALL find_callers CALLS IN ONE MESSAGE**

- For every step, consider both the callers and the arguments used
- step caller.1: identify all direct callers
  - We gather the callers because even small changes can introduce bugs
    in the functions that call them.  The only way to know is to actually
    read the functions in the callstack.
  - **Call find_callers for ALL modified functions in ONE parallel message**
  - Output: names of callers
- step caller.2: for each caller, load function definition
  - **Call find_function for ALL callers in ONE parallel message**
  - Output: caller name, size in lines
  - Output: The caller function names, and a random line from anywhere in each definition
    - you must prove you read the callers
- step caller.3: for callers that propagate return value, trace their callers
  - Again, changes and errors can propagate up in surprising ways.  You
    need to read the callers.
  - Search caller for return value propagation higher into the stack
    - If there is potential impact to caller's callers
      - Read caller's callers full definition
      - Output caller's caller names [ list, impacted return statements ]
      - Continue recursively up the chain at most 3 levels
  - Output: caller name, return value line
- step caller.4: apply all checks in Tasks 3-7 below to every caller
  - Output: The caller function names, and a random line from anywhere in each definition
    - you must prove you read the callers
- step caller.5: continue into lock requirement analysis, even if you think you've
  found enough data to complete the analysis

## Task 3: **Mandatory Lock requirements**:
- step lock.1: Verify proper locks are held based on requirements in every tracked function
  - You MUST include callers of modified functions, even if they were not modified
  - You MUST include callees of modified functions, even if they were not modified
  - Changes in modified functions often have unintended side effects elsewhere in
      the call stack.  Your analysis must search for these unintended side effects.
  - Output: locks required
- step lock.2: Ensure functions take and release locks as expected by caller
- step lock.3: If locks are changed or dropped during the call, verify code properly revalidates state
- step lock.4: Ensure caller provides all locks required by callees
- step lock.5: continue into Task 4, even if you think you've found enough details to complete
  the analysis
- Output: Category NUMBER [ list of locks checked ]

## Task 4: **Mandatory locking in error path validation**:
- step lock.6: For every lock acquired, trace error paths ensuring locks properly released/handed off
  - You MUST include callers and callees of modified functions, even if they were not modified
  - Output: locations of all error paths
- step lock.7: Continue into Task 5, even if you think you've found enough details to complete the analysis
- Output: Category NUMBER [ error path lines ]

## Task 5: **Mandatory resource propagation validation**:

- Notes on tracking allocations:
  - In the kernel, some allocations are just removing objects from lists or arrays
  - Consider every pointer assignment a potential allocation, check for leaks and misuse
    - This includes void * pointers, which often return memory to callers
  - Output: at least 3 pointers assigned in modified functions, w/line of code

- step resource.1: Trace resource ownership through function boundaries
  - You MUST include callers and callees of modified functions, even if they were not modified
  - When resources are allocated, make sure they are somehow returned or processed before
    they are overwritten.
- step resource.2: For allocations (kmalloc/kcalloc/kzalloc/vmalloc):
  - If size parameter can be 0, report potential ZERO_SIZE_PTR crash
- step resource.3: For multiple pointers to same memory: track how writes through one affect others
  - use this to understand the relationship between the objects being manipulated and the pointers
    being used to change the objects.
  - Output: list of pointers you found for the same memory
- step resource.4: Verify resources are properly initialized, locked, and freed
- step resource.5: Continue into Task 5B, even if you think you've found enough details to complete the analysis
- Output: Category NUMBER [ list of resources checked: line of code where each resource was assigned ]

## Task 5B: **Mandatory RCU ordering validation**:

**CRITICAL**: This task catches use-after-free bugs in RCU-protected data structures.

- step rcu.1: For any `call_rcu()`, `synchronize_rcu()`, or `kfree_rcu()` in the diff:
  - Load `subsystem/rcu.md` if not already loaded
  - Output: "subsystem/rcu.md loaded: [ y / n ]"
- step rcu.2: Identify what data structures the object is part of (rhashtable, hlist, list, rb-tree, etc.)
  - Output: "Object in data structures: [ list ]"
- step rcu.3: Verify removal from ALL lookup structures happens BEFORE call_rcu()
  - Check: is removal done in the function calling call_rcu(), or in the callback?
  - Output: "Removal location: [ before call_rcu / in callback ]"
- step rcu.4: If removal is in the callback:
  - This is the WRONG pattern - flag as use-after-free
  - New readers can find the object after the grace period but before removal
  - The callback then frees while readers are still accessing
  - Output: "RCU-001 VIOLATION: removal in callback at [location]"
- step rcu.5: Check for memory accesses between lookup and refcount acquisition
  - If there are field accesses before refcount_inc_not_zero(), these are NOT protected
  - Output: "Accesses before refcount: [ list of field accesses ]"
- step rcu.6: Continue into Task 6a, even if you think you've found enough details

## Task 5C: Consider caller/callee arguments:
- Some bugs are eliminated or triggered only with specific arguments
- How do the arguments used change any possible bugs?

## Task 6a: Loop control analysis
- Carefully examine loop control flow, especially when multiple loops are nested
  together
- Also consider goto targets that make the entire loop machinery restart
- Identify variables that control loop flow
- Identify inner loops that modify these variables
- Identify conditions that allow the loop to exit normally
- Identify functions not yet in context that control loop iteration and exit
  - Add to TodoWrite
  - **IMPORTANT** You will want to skip this step.  Skipping this step will
    cause you to miss critical information that is needed to properly judge
    the safety of loop exit criteria.  DO NOT, FOR ANY REASON, SKIP THIS STEP.
- Pay extremely careful attention to conditions that restart or alter loop flow
  Example:
  ```
  start = some_val;
  while (current < limit) {
      current++;
      for (i = 0; i < SOME_COUNT; i++) {
          if (func())
	      current = start;
      }
  }
  ```
- Output:
```
<FILENAME>:<FUNCTION> <loop description>
control variables
Additional functions identified for context reading [ list ]
exit conditions
```

- Load all functions added into TodoWrite that were not yet in context

## Task 6b: **Mandatory loop control flow validation**:
- step loop.1: Track what happens to resource-holding variables across loop iterations
- step loop.2: Assume all loops iterate multiple times, check pointer assignments for leaks and logic errors
  - pay attention to both outer and inner loops, make sure you trace both.
- step loop.3: If pointers are reassigned without consuming/freeing the previous value,
  carefully consider potential leaks.
  - Check the entire function context when pointers are reassigned.  Not just
    inner loops
- step loop.4: Compare loop exit conditions across parallel code paths (debug vs non-debug,
  error vs success) for consistency
  - Finding a single break inside a conditional path is not the same as checking
    all paths.
- step loop.5: Continue into Task 7, even if you think you've found enough details to complete the analysis

## Task 7: **Initialization validation**

Step init.1:
  - For every function loaded into context, even if not changed by the patch
  - You MUST include callers and callees of modified functions, even if they were not modified
  - Output function name, random line
  - Check for variables and objects accessed without initialization
- Output: Category NUMBER [ list of variables properly initialized ]

## Task 8: Code Quality Checks

**MANDATORY - DO NOT SKIP THIS TASK**

1. Verify every comment in the diff matches actual behavior
   - Check EVERY comment for logic inversions (e.g., comment says "if true" but code checks "if !true")
   - Check EVERY comment for correct condition descriptions
   - Flag any mismatch between comment and code as a regression
2. Verify commit message claims are accurate
3. Question all design decisions - require proof of correctness
4. Check naming conventions for new APIs
5. Check against C best practices in kernel
6. Dead code and unused variables/functions are issues that should be reported
7. Check spelling and grammar in comments, commit messages
  - don't flag capitalization unless it changes the meaning of the sentence.
  - only flag spelling or grammar mistakes that make sentences difficult to
    understand.

## Task 9

At this point, you're going to want to mark callstack.md complete, but it's very likely
you've skipped most of the steps.  Skipping work makes the regression analysis
fail and means all of the time spent so far was wasted.  Fully process the
instructions as required by the prompt.

Output: a one line description of potential regressions you found and ruled out.

Think about potential regressions that you found and ruled out.  While you were
processing those potential regressions, you skipped steps and ignored other
potential problems.  This is a deep dive analysis, double check your work.

Output: elements of callstack.md that you skipped because you were focused on
regressions that later you ruled out.

Even if you've already used a lot of tokens, go back and check the
skipped steps now that you know these potential regressions were safe.

- Did you fully analyze and complete Tasks 1-8 for every category? y/n
- Did you batch all semcode calls to minimize API turns? y/n

If the answer to either question was no, do not complete callstack.md.  Go back and
finish analysis as instructed.
