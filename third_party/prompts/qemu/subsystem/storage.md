# QEMU Storage Device Emulation Guidelines

## Core Principles
Storage emulators (SCSI, NVMe, IDE, AHCI) process complex guest command sets and DMA transfers against virtual block backends.

## Critical Invariants to Verify

1. **Mutable Guest Parameters**:
   - Device parameters such as sector size or logical block size may be dynamically modified by guest commands (e.g. SCSI `MODE SELECT`).
   - Requests already in flight must use their allocated request buffer size (`r->buflen`), NEVER the dynamically updated device-level block size, to prevent out-of-bounds heap access during command emulation (e.g. `WRITE SAME`).

2. **PRP / SGL List Validation (NVMe)**:
   - Validate that Physical Region Page (PRP) entries and Scatter-Gather Lists (SGL) do not point to unmapped memory or cause integer overflow when summing segment lengths.

3. **Request Lifecycle and Error Unwinding**:
   - When a storage command fails, ensure the allocated `SCSIRequest` or `NVMeRequest` is completed with an error status and deallocated.
   - Do not leak I/O vectors or DMA scatter-gather lists on error paths.
