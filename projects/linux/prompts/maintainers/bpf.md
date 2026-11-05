+++
title = "BPF"
paths = ["kernel/bpf/", "tools/lib/bpf/", "tools/testing/selftests/bpf/", "net/core/filter.c", "include/linux/bpf*.h"]
symbols = ["bpf_*"]
+++
# BPF: maintainer notes

Coding-style and selftest conventions the BPF maintainers expect in BPF kernel code, libbpf and BPF selftests, and how they judge verifier precision.

- BPF code uses the modern kernel multi-line comment style: the opening `/*` goes on its own line and the comment text starts on the next line; text on the same line as the opening `/*` of a multi-line comment is not accepted.
- Single-line comments that fit on one line (`/* This is fine. */`) are fine as-is.
- The comment style applies to all BPF-related paths (kernel/bpf/, net/core/filter.c, include/linux/bpf*.h, tools/lib/bpf/, tools/testing/selftests/bpf/ and others), even when surrounding code in the same file still uses the old style.
- In BPF selftests, use the ASSERT_*() macros (ASSERT_OK(), ASSERT_ERR(), ASSERT_EQ(), ASSERT_OK_PTR(), ASSERT_OK_FD(), etc., see tools/testing/selftests/bpf/test_progs.h) for all new tests and when updating existing tests.
- CHECK(), CHECK_FAIL() and CHECK_ATTR() are deprecated and must not be used in new tests or patches; ASSERT_*() has been the BPF selftest standard since 2020 (no global duration variable, type-specific macros, better actual-vs-expected messages).
- In the verifier (kernel/bpf/verifier.c and the analysis code it calls), a conservative outcome is a precision limitation, not a bug: no loop count, an extra verification pass, weaker state pruning, or a bound that errs on the safe side. Do not report it when the program is still verified safely and is handled no worse than before the patch. Report soundness issues (accepting an unsafe program, a bound or range that is too permissive) as usual, and report lost precision only when it makes the verifier reject, or hit its complexity limit on, programs that the code before the patch accepted.
