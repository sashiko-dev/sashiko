# systemd Technical Deep-dive Patterns

## Core Instructions

- Trace full execution flow, gather additional context from the call chain
- IMPORTANT: never make assumptions based on return types, checks, or comments -
  explicitly verify the code is correct by tracing concrete execution paths
- IMPORTANT: never skip any steps just because you found a bug in previous step
- Never report errors without checking if the error is impossible in the call path

## Error Handling

**Return Codes**:
- Error codes are returned as negative `Exxx`. e.g. `return -EINVAL`
- For constructors, returning `NULL` on OOM is acceptable
- For lookup functions, `NULL` is acceptable for "not found"
- Use `RET_NERRNO()` to convert libc style (-1/errno) to systemd style (-errno)

**Logging Rules**:
- "Library" code (`src/basic/`, `src/shared/`) must NOT log (except DEBUG level)
- "Main program" code does logging
- "Logging" functions should not log errors from other "logging" functions
- Use `log_error_errno(r, "message: %m")` for combined log and return
- Use `SYNTHETIC_ERRNO(E...)` when error is not from called function

**Assert Usage**:
- `assert_return()` - for public API parameter validation, returns error code
- `assert()` - for internal programming error detection, aborts
- Both only for programming errors, not runtime errors

**Ignoring Errors**:
- Cast to `(void)` when intentionally ignoring return values
- Example: `(void) unlink("/foo/bar/baz");`

## Memory Management

**Cleanup Attributes**:
- `_cleanup_free_` - auto-free with `free()`
- `_cleanup_close_` - auto-close file descriptors
- `_cleanup_fclose_` - auto-close FILE*
- `_cleanup_(foo_freep)` - custom cleanup with `foo_freep`

**Ownership Transfer**:
- `TAKE_PTR(p)` - transfer pointer ownership, sets p to NULL
- `TAKE_FD(fd)` - transfer fd ownership, sets fd to -EBADF

**Allocation Rules**:
- Always check OOM - no exceptions
- Use `log_oom()` in program code (not library code)
- Avoid fixed-size string buffers unless maximum is known and small
- Never use `alloca()` directly - use `alloca_safe()`, `strdupa_safe()`
- Never use `alloca_safe()` in loops or function parameters

## File Descriptors

**O_CLOEXEC Requirement**:
- ALL file descriptors must be O_CLOEXEC from creation
- `open()` must include `O_CLOEXEC`
- `socket()`/`socketpair()` must include `SOCK_CLOEXEC`
- `recvmsg()` must include `MSG_CMSG_CLOEXEC`
- Use `F_DUPFD_CLOEXEC` instead of `F_DUPFD`
- `fopen()` should use `"e"` flag

**Other FD Rules**:
- Never use `dup()` - use `fcntl(fd, F_DUPFD_CLOEXEC, 3)`
- The `3` avoids stdin/stdout/stderr (0, 1, 2)
- Use `O_NONBLOCK` when opening 'foreign' regular files

## Threading Rules

**CRITICAL - No Threads in PID1**:
- PID1 must NEVER use threads
- Cannot mix malloc in threads with clone()/clone3() syscalls
- Risk of deadlock: child inherits locked malloc mutex
- Fork worker processes instead of worker threads
- Use `posix_spawn()` which combines clone() + execve()

**Thread Safety**:
- Library code should be thread-safe
- Use TLS (`thread_local`) for per-thread caching
- Use `is_main_thread()` to detect main thread
- Disable caching in non-main threads

## NSS and Deadlock Prevention

**No NSS from PID1**:
- Never issue NSS requests (user/hostname lookups) from PID1
- NSS may synchronously talk to services we need to start
- Risk of deadlock

**No Synchronous IPC from PID1**:
- Do not synchronously talk to any service from PID1
- Risk of deadlocks

## Coding Style

**Naming Conventions**:
- Structures: `PascalCase` (except public API)
- Variables and functions: `snake_case`
- Return parameters: prefix with `ret_` (success) or `reterr_` (failure)
- Command line variables: prefix with `arg_`

**Destructor Patterns**:
- Destructors must accept NULL and treat as NOP (like free())
- Destructors should return same type and always return NULL
- Enables: `p = foobar_unref(p);`
- Destructors deregister from larger object, not vice versa
- Destructors must handle half-initialized objects

**Naming Destructors**:
- `xyz_free()` - full destruction, frees all memory
- `xyz_done()` - destroys content, leaves object allocated
- `xyz_clear()` - like done(), but resets for reuse
- `xyz_unref()` - decrement refcount
- `xyz_ref()` - increment refcount

## Type Safety

**Preferred Types**:
- Use `unsigned` not `unsigned int`
- Use `char` only for characters, `uint8_t` for bytes
- Never use `short` types
- Never use `off_t` - use `uint64_t`
- Use `bool` internally, `int` in public APIs (C89 compat)
- Use `double` over `float` (unless array allocation)

**Time Values**:
- Always use `usec_t` for time values
- Don't mix usec/msec

## Functions to Avoid

- `memset(..., 0, ...)` -> use `memzero()` or `zero()`
- `strcmp()` -> use `streq()` when checking equality
- `strtol()`/`atoi()` -> use `safe_atoli()`, `safe_atou32()`
- `htonl()`/`ntohl()` -> use `htobe32()`, `htobe16()`
- `inet_ntop()` -> use `IN_ADDR_TO_STRING()` macros
- `dup()` -> use `fcntl(fd, F_DUPFD_CLOEXEC, 3)`
- `fgets()` -> use `read_line()`
- `exit()` -> propagate errors up, use `_exit()` in forked children
- `basename()`/`dirname()` -> use `path_extract_filename()`/`path_extract_directory()`
- `FILENAME_MAX` -> use `PATH_MAX` or `NAME_MAX`

## Control Flow

**goto Usage**:
- Only use `goto` for cleanup
- Only jump to end of function, never backwards

**Loops**:
- Use `for (;;)` for infinite loops, not `while (1)`
