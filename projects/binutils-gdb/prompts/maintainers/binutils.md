+++
title = "binutils tools (objdump, readelf, nm, ar, strings)"
paths = ["binutils/"]
symbols = []
+++
# binutils tools: maintainer notes

- These tools are display and archive front ends over BFD: a bug in the tool is usually a missing check or a wrong assumption about BFD data, not a reason to special-case one file.
- A display change must keep the output stable for existing scripts; tests compare exact objdump/readelf output and DejaGnu expected results.
- When a new option or output column is added, update the tool's documentation and the testsuite in the same patch.
- Parsing of untrusted archives and files must not trust header sizes; report an out-of-bounds read even if the file is "obviously malicious".
- Do not silence a diagnostic to make malformed input pass; the tool should fail cleanly.
