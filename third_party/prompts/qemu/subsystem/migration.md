# QEMU Migration and VMState Review Guidelines

## Core Principles
QEMU live migration serializes VM state across processes and versions via `VMStateDescription`.

## Critical Invariants to Verify

1. **`post_load` Validation**:
   - Deserialized state originates from the network or saved files and is untrusted.
   - Any restored index into arrays must be validated:
     ```c
     if (s->current_queue >= s->max_queues) {
         error_report("Corrupt migration data: current_queue exceeds maximum");
         return -EINVAL;
     }
     ```
   - If state restoration triggers dynamic allocation (e.g., indirection tables, packet buffers), verify that length fields are non-zero, within acceptable maximum limits, and successfully allocated before use.

2. **Subsection Versioning & Backward Compatibility**:
   - Modifying a `VMStateDescription` directly changes the wire protocol for that device.
   - Any new fields added to existing devices must be placed inside a subsection with a `.needed` predicate:
     ```c
     static bool my_feature_needed(void *opaque) {
         MyDeviceState *s = opaque;
         return s->feature_enabled != 0;
     }
     ```
   - Incrementing `version_id` breaks backward migration to older versions of QEMU. Ensure machine type compatibility (`hw_compat_*`) is respected.
