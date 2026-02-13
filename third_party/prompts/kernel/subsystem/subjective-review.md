# Subjective Review

**Risk**: None, subjective code quality assessment

**When to check**: mandatory for all non-trivial commits

IMPORTANT: never flag single dumb grammar changes: ex its vs it's, unless you
have a collection of them that make the commit message/comment hard to understand

Place each step defined below into TodoWrite.

**Basic validation**
- step 1: Is the code clean and easy to understand?
  - the kernel is full of complex and difficult code, compare new changes with nearby code when judging
- step 2: Is there a less complex approach that could solve the problem?

**Mandatory code duplication analysis**
- step 3: Look for duplicated code inside the changes being made
- step 4: Look for existing related functions that have been duplicated by the diff

**Mandatory API analysis**
- step 5: If new APIs are introduced
  - Output: new API name, location of major calls
  - are they consistent in structure and expectations with existing APIs
  - are the functions consistent in naming and argument order
  - are the locking and allocation/free expectations clear and well documented?
- step 6: If existing APIs are used
  - Output: existing API name, location of major calls
  - is the usage diving into internal/private fields that are not meant for consumption?

**Mandatory commit message validation**
- step 7: Compare commit title and message against actual code changes
  - Output: Commit title, line count of message
- step 8: Verify the changelog is complete (describes all significant changes)
- step 9: Verify the changelog is concise (no unnecessary verbosity)
- step 10: Check that the "why" is explained, not just the "what"
- step 11: Flag missing context that would help reviewers/maintainers

**After analysis:** Issues found: [none OR list]
