# Testability and Test Review

General guidance for judging whether a change is testable and whether the tests
it carries are worth having. Framework-specific rules live in `kunit.md` and
`selftests.md`, which load only when the patch touches them.

## When Absent Tests Are Worth Raising

Most kernel patches ship without tests, and that is a normal state rather than a
defect. Raise a concern about missing tests only where one is both practical and
would have caught something:

- A bug fix whose commit message describes a concrete reproducer, in code that
  an existing test suite already reaches.
- A new UAPI surface: a syscall, an ioctl, a sysfs or debugfs file, a netlink
  attribute. Interfaces that userspace will depend on are the highest-value
  place for a test, because a regression there is a contract break.
- A change to a pure, self-contained algorithm (parsing, allocation policy,
  hashing, bitmap or list manipulation) where a unit test is cheap.
- A patch that fixes a bug in code that already has a test suite, where the
  existing suite passed both before and after. That is direct evidence the
  suite has a hole.

Do not raise it for:

- Refactors with no behavioural change, comment and typo fixes, or cleanups.
- Driver code for hardware with no emulation or model available.
- Changes whose only failure mode needs a specific machine, timing window, or
  physical device to observe.
- Error paths reachable only under allocation failure, unless the subsystem has
  established fault-injection coverage.

When you do raise it, say what the test would check, not merely that one is
missing. "No test" is not review feedback; "nothing exercises the new
`E2BIG` path, which the previous version returned `EINVAL` for" is.

## Reviewing Tests That Are Present

Treat test code as code. The bar is not lower because a bug there does not
panic the kernel; a broken test is worse than no test, because it advertises
coverage that does not exist.

### Does it actually test the change?

The single most valuable check: **would this test have failed before the
patch?** A test added alongside a fix should exercise the changed path. Read
the test against the diff and confirm they meet. A test that passes with the
fix reverted is decoration.

Watch for tests that assert something trivially true: checking a function
returns without error while ignoring what it returned, comparing a value to
itself, or asserting on a condition the setup code just established.

### Does it cover both directions?

The most common gap in an otherwise sound test is that it only proves the happy
path: valid input produces the right answer, and nothing checks what invalid
input does. Both halves are needed, and the error half is where the bugs
usually are, because it is the half that runs least often in production.

- **Is there a case for each way the code can refuse?** Read the new
  conditionals in the diff. Every bounds check, validation branch and early
  error return the patch adds should have a case that trips it. A check that no
  test exercises is untested code that looks tested.
- **Does it assert the specific error, or only that one occurred?**
  `KUNIT_EXPECT_LT(test, ret, 0)` passes for any failure, including the wrong
  one. Assert the exact value: `-EINVAL`, not "negative". For anything
  reachable from userspace this matters twice over, since the errno is part of
  the interface, and a test that accepts any error will not notice when it
  changes out from under a caller.
- **Are the boundaries tested, or only the middle?** Where the code compares
  against a limit, the interesting values are the limit itself and one on each
  side of it. Zero, one, empty and NULL are the other perennial gaps. An
  off-by-one lives exactly where a test built from typical values will not
  look.
- **Are the arithmetic edges covered where they apply?** Values that overflow
  when multiplied by an element size, negative values passed into unsigned
  parameters, and anything near `INT_MAX`, `SIZE_MAX` or `PAGE_SIZE` are the
  cases that become exploitable bugs rather than merely wrong answers.

This asks for one or two more cases, not for exhaustive coverage. Name the
specific missing one -- "nothing passes a length of zero, which the new
`if (len < 1)` rejects" -- rather than asking for "more negative testing".

### Does it test behaviour or implementation?

Prefer tests that pin observable behaviour: return values, errno, emitted
events, resulting state visible through an interface. Tests that assert on
internal structure layout, call counts, or the exact order of internal
operations break on every legitimate refactor and get deleted rather than
fixed.

### Failure modes to look for

- **Silent skips.** A test that skips when its prerequisites are missing is
  correct; one that *passes* in that case is not. Check that unavailable
  dependencies produce a skip result, not success.
- **Resource leaks.** Tests allocate, open files, create namespaces, load
  modules. Check the cleanup path runs on failure too, not just on the success
  path, and that a failing assertion does not leave the system dirty for the
  next test.
- **Order dependence.** A test that only passes after another test has run, or
  that leaves global state behind, will fail confusingly when the suite is run
  in isolation or in parallel.
- **Timing.** `sleep` as synchronisation is a flake generator. Look for a
  condition to wait on instead. Tests that assert on elapsed time will fail on
  loaded CI machines.
- **Hardcoded environment.** Absolute paths, fixed PIDs or ports, assumptions
  about page size, core count, `CONFIG_` options, architecture word size, or
  running as root. Privileged operations should check for the capability and
  skip when it is absent.
- **Ignored return values.** Setup calls whose failure is not checked turn a
  broken environment into a confusing assertion failure much later.

### Test quality signals

- A failing assertion should say what was expected and what was seen. An
  assertion that prints only "failed" costs the next person a debugging
  session.
- Test names should describe the property under test, so a failure in CI output
  is meaningful without reading the source.
- New test files must be wired into the build, or they will never run. This is
  the most common way a test is silently absent despite being merged.

## Where a Test Belongs in a Series

When the patch under review is part of a series, the context lists the patches
that come **after** it. It does not list the ones before it, so judge ordering
only from what follows, and say nothing when there are no subsequent patches or
when the patch is being reviewed on its own.

Two orderings are worth raising:

- **A test that lands before the fix it exercises.** If this patch adds a test
  for a bug and a later patch in the series fixes that bug, every commit
  between the two fails that test. That breaks bisection: someone landing on an
  intermediate commit sees a failure unrelated to what they are hunting. The
  fix should come first, or the fix and its test should be a single patch.

- **A refactor whose tests arrive afterwards.** If this patch restructures code
  without intending a behavioural change, and a later patch adds tests for that
  same code, the series never demonstrates the property the refactor claims.
  Tests added first, passing against the original implementation and again
  after the restructuring, are what make "no functional change intended"
  checkable rather than asserted. Ask for the test patch to move ahead of the
  refactor.

The two rules point in opposite directions on purpose: a test for existing
behaviour belongs before the change, and a test for behaviour that does not
work yet belongs with or after the fix. What distinguishes them is whether the
test would pass at the commit it is introduced in. A test that cannot pass when
it lands is the problem in both cases.

Series ordering is Low severity as a rule, since it is a question of hygiene
rather than correctness. Bisectability is a real cost, though, and worth a
sentence when a failing test would sit in the tree across several commits.

## Reporting

Testability findings are usually Medium at most, and often Low. A test that
passes when it should fail is the exception worth arguing for: it is an active
false signal, not merely a gap. Weigh a missing test against how much of the
subsystem already goes untested; consistency with surrounding practice matters
more than an absolute standard.
