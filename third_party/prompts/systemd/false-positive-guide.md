# False Positive Elimination Guide

## Purpose
This guide helps eliminate false positives before reporting regressions.
Apply these checks to every potential issue found.

## Core Principle
**Never report a bug you cannot prove with concrete code paths.**

## Verification Checks

### CHECK 1: Can the Code Path Actually Execute?

Before reporting an issue:

1. **Trace the call path** from entry point to problematic code
2. **Identify all conditions** that must be true to reach it
3. **Verify conditions are possible** in real usage

**False Positive Example**:
```c
/* Reported: NULL dereference of p */
void foo(SomeStruct *s) {
        if (!s)
                return;
        /* ... lots of code ... */
        use(s->member);  /* s CANNOT be NULL here! */
}
```

### CHECK 2: Are Prerequisites Validated Elsewhere?

systemd often validates at API boundaries, not at every use.

**Check**:
- Does the public API validate this input?
- Is this internal code that assumes valid input?
- Are there assertions documenting the assumption?

**False Positive Example**:
```c
/* Internal function - caller guarantees non-NULL */
static void internal_helper(Unit *u) {
        /* No NULL check needed - callers must ensure u != NULL */
        use(u->id);
}
```

### CHECK 3: Is This Defensive Programming Territory?

Don't recommend checks that can't fail:

**False Positive Example**:
```c
/* Reported: Should check malloc return */
p = malloc(0);  /* malloc(0) returns NULL or unique ptr - both valid */
```

**Real Issue Example**:
```c
p = malloc(size);
/* size is user-controlled, could be huge */
if (!p)  /* This check IS needed */
        return -ENOMEM;
```

### CHECK 4: Does Error Handling Actually Matter?

Some error paths are intentionally ignored:

```c
/* Intentional - cleanup best-effort */
(void) unlink(temp_file);  /* Cast to void = intentionally ignored */
```

### CHECK 5: Is the Order Actually Wrong?

LIFO cleanup order issues need proof:

**Not an issue**:
```c
_cleanup_free_ char *a = strdup("a");  /* First */
_cleanup_free_ char *b = strdup("b");  /* Second */
/* Order doesn't matter - a and b are independent */
```

**Real issue**:
```c
_cleanup_free_ Object *obj = NULL;  /* First - cleaned last */
_cleanup_(lock_release) Lock *l = take_lock();  /* Second - cleaned first */
obj = alloc_under_lock();
/* Issue: obj freed AFTER lock released, but alloc was under lock */
```

### CHECK 6: Is This a Test/Debug Path?

Test code has different standards:

- Memory leaks acceptable in test programs
- Assertions in tests are expected to fire on bad input
- Debug code may have intentional simplifications

### CHECK 7: Version/Feature Check

Some code only runs with specific features:

```c
if (!HAVE_FEATURE)
        return;

/* Code below only runs if HAVE_FEATURE defined */
feature_specific_code();
```

### CHECK 8: Cleanup Attribute Compatibility - Double Check

For cleanup issues specifically:

1. **Verify the cleanup function** - read its implementation
2. **Verify what values it handles** - NULL? Error codes?
3. **Verify all code paths** - what value does variable hold at each return?

**Not an issue** (most cleanups handle NULL):
```c
_cleanup_free_ char *p = NULL;
if (condition)
        p = strdup(x);
return 0;  /* free(NULL) is safe */
```

## Elimination Process

For each potential issue:

1. [ ] Can I show the exact code path that triggers this?
2. [ ] Have I verified the path is actually reachable?
3. [ ] Is this truly a bug, not defensive programming request?
4. [ ] Have I checked for validation elsewhere in the call chain?
5. [ ] Is this production code, not test/debug?

**If any answer is NO or UNCERTAIN**, do not report the issue.

## Output Format

For issues that pass all checks:

```
VERIFIED ISSUE: [brief description]
Code path: function_a() -> function_b() -> issue_site
Condition: [what must be true for this to trigger]
Evidence: [code snippet showing the problem]
```

For eliminated false positives:

```
ELIMINATED: [brief description]
Reason: [which check failed and why]
```
