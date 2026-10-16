+++
title = "Perf Tools"
paths = ["tools/perf/"]
symbols = ["openat", "fdopendir", "closedir"]
+++
# Perf Tools: maintainer notes

Maintainer preferences and conventions for patches to the user-space perf tool under tools/perf/.

- Discourage adding new logic to tools/perf/arch/; keep that directory for code strictly tied to host execution and prefer cross-platform implementations, so a perf binary on one architecture can analyze perf.data recorded on another.
- Handle architectural differences in recording or analysis by checking the ELF machine (e_machine) dynamically rather than via host-arch-specific code.
- perf is built against both glibc and musl: explicitly include the POSIX-specified header for every libc function, variable or constant used, and do not rely on headers being pulled in implicitly.
- Place required system and POSIX headers explicitly at the top of the file; do not hand-write forward declarations of standard libc functions or types.
- In perf headers, prefer forward declarations (e.g. struct evlist;) over full header inclusions when only pointers to the struct are referenced.
- Avoid ERR_PTR in new perf user-space code; functions returning pointers should return NULL on failure.
- When a specific error code must reach the caller, return a negative errno int and pass the object back through a double-pointer argument, or set errno and return NULL.
