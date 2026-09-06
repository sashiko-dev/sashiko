# KUnit Subsystem Details

In-kernel unit testing framework. Tests are built into the kernel or as modules
and run in-kernel, typically under UML or QEMU via `./tools/testing/kunit/kunit.py run`.

Distinct from kselftest (`tools/testing/selftests/`), which runs in userspace
and tests the kernel from outside. A patch may touch both; they have different
rules.

## The In-Tree Style Guide Is Authoritative

Naming, Kconfig entries, and test file and module layout are specified in
`Documentation/dev-tools/kunit/style.rst`. That file is supplied to you in the
`<in_tree_style_guides>` section, read from the tree under review, so a patch
that amends the style guide is judged against its own updated rules.

**Check every question of naming, configuration and file placement against
that text, not against memory and not against the examples below.** It is the
document the people reading your review will cite back at you. If the section
is absent, the tree predates the document or is not Linux: fall back to the
conventions visible in neighbouring tests, and say so in the finding.

### Questions it answers -- answer each one

Work through this list against the supplied text rather than reporting only
what happens to catch your eye. Each question has an answer in that document;
find it and compare it to the patch.

- Where does a test source file belong, relative to the code it tests, and is
  this file there?
- What should the file itself be named?
- What should the module be named, if it builds as one?
- What should the Kconfig symbol be named, given the suite name?
- What should the suite be named, given the subsystem it belongs to?
- What should the individual test cases be named?

The document phrases some rules as requirements and others as recommendations.
Do not treat that as a licence to skip the recommendations: they describe the
convention the subsystem already follows, so a patch departing from one is
worth raising as a question even where nothing enforces it. File placement in
particular is written as advice and is still the convention.

The rest of this guide covers what that document does not: the semantics that
make a KUnit test silently wrong rather than merely unidiomatic.

## Assertions vs Expectations

The most consequential distinction in KUnit, and the most common review finding.

| Macro family | On failure |
|--------------|-----------|
| `KUNIT_EXPECT_*` | Marks the test failed and **continues executing** |
| `KUNIT_ASSERT_*` | Marks the test failed and **aborts the test case** |

The rule: if the code after the check would be invalid when the check fails,
it must be an assertion. The canonical bug is expecting a pointer to be
non-NULL and then dereferencing it:

```c
/* WRONG: on failure the test continues and dereferences NULL */
KUNIT_EXPECT_NOT_ERR_OR_NULL(test, obj);
KUNIT_EXPECT_EQ(test, obj->field, 42);

/* CORRECT: abort before the dereference */
KUNIT_ASSERT_NOT_ERR_OR_NULL(test, obj);
KUNIT_EXPECT_EQ(test, obj->field, 42);
```

Use `KUNIT_EXPECT_*` for independent checks, so one failure still reports the
others. Use `KUNIT_ASSERT_*` for preconditions the rest of the case depends on:
allocation results, setup success, non-NULL pointers, and successful lookups.

Common macros: `KUNIT_EXPECT_EQ/NE/LT/LE/GT/GE`, `KUNIT_EXPECT_TRUE/FALSE`,
`KUNIT_EXPECT_NULL/NOT_NULL`, `KUNIT_EXPECT_PTR_EQ/NE`,
`KUNIT_EXPECT_STREQ/STRNEQ`, `KUNIT_EXPECT_MEMEQ/MEMNEQ`,
`KUNIT_EXPECT_NOT_ERR_OR_NULL`, and the matching `KUNIT_ASSERT_*` forms. Each
takes `struct kunit *test` as its first argument. The `_MSG` variants append a
format string and should be used where the values alone will not identify which
iteration failed.

`KUNIT_FAIL(test, fmt, ...)` fails unconditionally, for unreachable branches.

## Test and Suite Structure

```c
static void foo_parse_rejects_empty_input(struct kunit *test)
{
	...
}

static struct kunit_case foo_test_cases[] = {
	KUNIT_CASE(foo_parse_rejects_empty_input),
	KUNIT_CASE_PARAM(foo_parse_accepts_all_widths, foo_width_gen_params),
	{}
};

static struct kunit_suite foo_test_suite = {
	.name = "foo",
	.init = foo_test_init,
	.exit = foo_test_exit,
	.test_cases = foo_test_cases,
};
kunit_test_suite(foo_test_suite);

MODULE_DESCRIPTION("KUnit tests for foo");
MODULE_LICENSE("GPL");
```

The case names above are named for the function under test and the codepath
exercised, which is what `style.rst` asks for. Check the names in a patch
against that file rather than against this example, and note in particular that
a trailing `_test` on a case name is not the convention.

Review points:

- The `kunit_case` array **must** be NULL-terminated with `{}`. Omitting it
  runs off the end of the array.
- `.name` is what `kunit.py` filters and reports on, and must be unique across
  the kernel. `style.rst` governs what it should be called.
- Test functions are `static void f(struct kunit *test)`. A non-static test
  function is a namespace leak, and `style.rst` notes that test names can
  collide with the C identifiers they test.
- `.init` runs before *each* case, `.exit` after each; they are not
  once-per-suite. State that must not leak between cases belongs there.
  `.suite_init` and `.suite_exit` are the once-per-suite forms.
- `kunit_test_suite()` registers one suite; `kunit_test_suites()` takes
  several. A suite that is defined but never registered never runs, and
  nothing warns about it.

## Memory and Cleanup

Prefer the test-managed allocators, which free automatically when the case
ends, including when it aborts through an assertion:

- `kunit_kmalloc(test, size, gfp)` / `kunit_kzalloc(test, size, gfp)`
- `kunit_kfree(test, ptr)` for early release
- `kunit_add_action(test, action, ctx)` to register arbitrary cleanup

Plain `kmalloc()` in a test leaks whenever an assertion aborts the case before
the matching `kfree()`, because the abort does not unwind. Flag `kmalloc()`
followed by `KUNIT_ASSERT_*` and a later `kfree()` on the same path.

Allocation results must be checked with `KUNIT_ASSERT_NOT_ERR_OR_NULL()`, not
expectations, for the reason above.

## Why the Build Wiring Matters

The exact Kconfig form belongs to `style.rst`; what matters for review is the
consequence of getting it wrong, because neither failure is loud:

- A test whose Kconfig entry does not follow the `KUNIT_ALL_TESTS` convention
  is not picked up by all-tests builds. It compiles, it is correct, and it
  never runs in CI.
- A new test file with no `obj-$(CONFIG_...)` line in the Makefile is never
  built at all. Nothing warns.
- A tristate test built as a module needs `MODULE_LICENSE()` and
  `MODULE_DESCRIPTION()`. modpost raises an error for a missing license and a
  warning for a missing description, so the first breaks the build for whoever
  enables the test as a module -- often not the author, who built it in.

So when a patch adds a test, check that it is reachable: a Kconfig entry
matching the documented pattern, a Makefile line referring to the same symbol,
and, where the subsystem uses one, an entry in the relevant `.kunitconfig`.

## Parameterised Tests

`KUNIT_CASE_PARAM(test_fn, gen_params_fn)` runs one case per parameter. The
generator returns the next parameter or NULL to stop, and should call
`kunit_param_desc()` (or set the description buffer) so failures identify which
parameter failed. A parameterised test whose failures all report the same name
is very hard to debug.

## Skipping

`kunit_skip(test, fmt, ...)` marks a case skipped and stops it; `kunit_mark_skipped()`
marks it skipped but continues. Use these when a precondition is genuinely
unavailable, such as a required feature not compiled in. Do not silently
`return` from a test when its preconditions are missing, since that reports a
pass for something that never ran.

## Quick Checks

- **`KUNIT_EXPECT_*` before a dereference**: should be `KUNIT_ASSERT_*`
- **Missing `{}` terminator** in the `kunit_case` array
- **Suite defined but no `kunit_test_suite()`** registration
- **`kmalloc()` in a test** where `kunit_kzalloc()` would free on abort
- **Kconfig missing the `KUNIT_ALL_TESTS` idiom**, so the test never runs in CI
- **New test file with no `obj-$(CONFIG_...)` Makefile line**, so it never builds
- **Missing `MODULE_LICENSE()`** in a test that can build as a module: modpost
  errors out. A missing `MODULE_DESCRIPTION()` warns
- **Non-static test functions or suite structures**
- **File placement**: is the test source where the style guide puts it,
  relative to the code under test?
- **Naming and Kconfig form**: check the supplied `style.rst` text, not memory,
  and answer each of its questions above rather than only the obvious one
- **Early `return` instead of `kunit_skip()`** when a precondition is missing
- **`.init` used for setup that should be once-per-suite** (`.suite_init`)
