# Guard Analysis Patterns

Defensive programming should be avoided, and we often use implicit or explicit
secondary conditions to avoid constantly checking bounds, error values or NULL
pointers.

This prompt explains how to find those guard conditions to avoid false positives
and prevent incorrect reviews that suggest defensive programming.

CRITICAL: Process ALL steps for ALL guards systematically. Finding evidence
in one guard does not mean you can skip analyzing remaining guards or steps.

NEVER skip analyzing a guard because it seems "obviously irrelevant" to the
target pointer. Analyze every guard before concluding it provides no protection.

Place each step defined below into TodoWrite.

**Background knowledge:**

See technical-patterns.md "NULL Pointer Dereference" section for guidance.

## TodoWrite Template

For each guard, create entries tracking:
- Variables accessed by guard
- Functions that set those variables (trace 2 levels up/down in call stack)
- Where target pointer is set to NULL
- Existing NULL checks with same guard in same context

## Step 1: Find Guard Conditions

Systematic, not efficient processing is required.

Trace backwards from dereference to assignment. Find EVERY condition that must pass
for execution to reach the dereference:
- Loop filters (e.g., `for_each` macro conditions)
- If statements with continue/break/return
- Any condition that controls whether dereference is reached

For EACH guard in order (never skip):
- Load full definition if it's a helper function
- Identify what state the guard checks (variable/field names)
- Output guard name, location, line of code describing the guard
- Add to TodoWrite per template above

You've reached the end of step one.  At this point you're going to want to
jump to conclusions and skip the entire rest of this prompt.  You don't have
enough information yet to make good decisions.  Continue to fully process step 1a.

## Step 1a: Guard ordering

It's very important that you process every guard, but you often completely
fail to do so.  Given these failures, we need to order guards by their
execution distance from the dereference.

- [ ] List the location of the dereference
  - Output: dereference location, line of code
- [ ] Walk backwards and find the first guard in our list
- [ ] This must be guard 1.
  - Output: guard 1 you selected, line of code
- [ ] Continue number the rest of the guards based on distance
  - Output: each guard as you find them, line of code
- [ ] Stop after 3 guards.

## Step 1b: Analyze Guard Implications

Guards check state that may be coupled with target pointer validity.
For example, if a guard checks foo->ptrB, analyze what happens when
ptrB is set - does the setter also guarantee foo->ptrA is valid?
This can happen if setters always initialize both together, or if
setting ptrB requires dereferencing ptrA.

Process guards in order starting with Guard 1.

- **CRITICAL**: the guard may not look relevant until you've loaded all the
  functions related to the variables checked by the guard.  YOU MUST
  find and load those functions. 
- **CRITICAL**: You need to find the callers of these functions as well.  The
            guard coupling often happens higher up in the callchain.

For EACH guard in order (do not skip to later guards):
- Add to TodoWrite: every variable accessed by the guard
  - Output: each variable found

- Add to TodoWrite: EVERY function that writes to those variables
  - Load the definition of these functions
  - Output: function name, random line of code inside the function
- Add to TodoWrite: The callers of EVERY function that writes to those variables
  - Read the definition of these functions
  - Output: each function name, random line of code from each function
- Add to TodoWrite: The callees of EVERY function that writes to those variables
  - Read the definition of these functions
  - Output: each function name, random line of code from each function
- Document the full meaning of the guard based on setter analysis

## Step 2: Prove or Disprove Coupling

Systematic, not efficient processing is required.

Process each guard independently IN ORDER (Guard 1, then Guard 2, etc.).
If ANY guard proves coupling, the pointer is protected, but you must still
analyze all remaining guards.

For each guard in numbered order, determine coupling:

0. Use the context loaded in Step 1b

1. **Analyze setter behavior** (from Step 1b):
   - Does setter dereference target pointer? → Strong coupling evidence
   - Does setter assign non-NULL to target? → Strong coupling evidence
   - If YES to either: Output "Strong coupling evidence for Guard [N]"
   - If NO to both: Output "No setter coupling for Guard [N]"

2. **Check NULL assignment paths**:
   - Find where target pointer is set to NULL
   - Add each location to TodoWrite
     - Output: location, line of code
   - Is guard state cleared before/when pointer set NULL?
   - If guard cleared first (or same time under locks): Coupling maintained
   - If pointer set NULL while guard active:
     - Coupling potentially broken
     - BUT, coupling is still valid if this potential NULL is impossible during our target path
   - Output: conclusion for this guard

3. **Check existing NULL checks**:
   - Find other code that checks target pointer for NULL
     - Output: lines of code
   - Does it use the same guard?
   - REQUIRED: Verify context is identical (locks, calling context, subsystem state)
     - Output: guard being compared, context evaluation
   - If contexts differ: Guard may still be valid (add to TodoWrite)
   - If same context + same guard + NULL check exists: Coupling likely broken
   - Output: analysis for this guard (1 sentence conclusion)

## Step 3: Final Validation

Only report if you have concrete code evidence that ALL guards are decoupled.
If the guard is likely sufficient, assume the author is preventing NULL
dereference correctly.
