# Review Report Template

## Instructions

When regressions are found, create `review-inline.txt` following this template.

## Formatting Rules

1. **Plain text only** - no markdown, no backticks for code
2. **Wrap at 78 characters** - except code snippets
3. **No line numbers** - use function names and file paths
4. **No dramatic language** - factual descriptions only
5. **Suitable for GitHub PR comment** or mailing list

## Template

```
Subject: Re: [PATCH] <original subject line>

I found potential issues in this patch:

=== Issue 1: <Brief description> ===

File: <filename>
Function: <function_name>()

The change introduces <describe the issue>.

Current code:

    <code snippet showing the problem>
    <use indentation, no backticks>

The issue is that <explain why this is wrong>.

This can be triggered when <describe the condition>.

Suggested fix:

    <code snippet showing fix if known>

---

=== Issue 2: <Brief description> ===

[Repeat format for each issue]

---

Analysis notes:
- <Any relevant context>
- <Call paths traced>
- <Assumptions verified>
```

## Example

```
Subject: Re: [PATCH] core: add new option FooBar=

I found potential issues in this patch:

=== Issue 1: Missing cleanup on error path ===

File: src/core/unit.c
Function: unit_load_fragment()

The change allocates memory for the new FooBar option but doesn't
free it on the error path at line where we return -ENOMEM.

Current code:

    int unit_load_fragment(Unit *u) {
            _cleanup_free_ char *foobar = NULL;

            foobar = strdup(value);
            if (!foobar)
                    return -ENOMEM;

            r = parse_something(foobar);
            if (r < 0)
                    return r;  /* foobar freed correctly */

            u->foobar = foobar;  /* ownership transferred */
            return 0;  /* BUG: foobar still has cleanup! */
    }

The issue is that TAKE_PTR() is not used when storing foobar
in the unit structure, so it will be double-freed.

This can be triggered by any unit file with a FooBar= setting.

Suggested fix:

    u->foobar = TAKE_PTR(foobar);
    return 0;

---

Analysis notes:
- Traced call path from unit_load() to unit_load_fragment()
- Verified _cleanup_free_ calls free() which cannot handle double-free
- Confirmed u->foobar is later freed in unit_free()
```

## Checklist Before Writing Report

- [ ] Issue verified against false-positive-guide.md
- [ ] Code path is actually reachable
- [ ] Concrete evidence provided (code snippets)
- [ ] No speculative issues included
- [ ] Formatting rules followed

## File Creation

Create the file in the current directory:
```
./review-inline.txt
```

Verify the file exists after creation.
