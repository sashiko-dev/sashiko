+++
title = "Tracing"
paths = ["kernel/trace/", "include/trace/", "include/linux/tracepoint.h"]
symbols = ["trace_*", "TRACE_EVENT*", "tracepoint_*"]
+++
# Tracing: maintainer notes

Conventions the tracing community expects for new or changed tracepoints.

- Tracepoint names follow the `subsystem_event` naming convention.
