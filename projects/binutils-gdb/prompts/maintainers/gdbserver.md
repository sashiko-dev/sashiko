+++
title = "gdbserver"
paths = ["gdbserver/"]
symbols = []
+++
# gdbserver: maintainer notes

- GDB and gdbserver must agree on the remote protocol: a packet format or state change needs the matching GDB client change and a test.
- Remote packets arrive from an untrusted peer: validate lengths, register numbers and thread ids before using them.
- Thread and process lifetime changes must be checked against both the all-stop and non-stop models.
- A behavior change needs a test in gdb.server/ or the matching testsuite directory.
