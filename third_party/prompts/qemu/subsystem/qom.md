# QOM (QEMU Object Model) Review Guidelines

## Core Principles
The QEMU Object Model provides dynamic typing, multiple inheritance through interfaces, and lifecycle tracking for devices.

## Critical Invariants to Verify

1. **`TypeInfo` Sizing**:
   - When registering a device or SoC subclass that defines its own class struct (`typedef struct MyClass MyClass;`), you MUST set:
     ```c
     .class_size = sizeof(MyClass),
     ```
   - Omitting `.class_size` causes QOM to use the parent's `class_size`. When `class_init` writes to subclass fields, it corrupts adjacent heap memory.
   - Verify that `.instance_size = sizeof(MyState)`.

2. **Phase Separation**:
   - `instance_init`:
     - Only initializes data structures, links, properties, and default values.
     - Must NOT allocate host OS resources (file descriptors, sockets) or register address spaces.
     - Must NOT be fallible (cannot return an Error).
   - `realize`:
     - Validates configured properties.
     - Allocates hardware-visible resources, registers MMIO, connects IRQs.
     - Must propagate all errors through `errp`. If any child realization or step fails, unwind all previously allocated resources before returning.
   - `unrealize`:
     - Cleanly dismantles everything done in `realize`.
     - Free dynamically allocated memory, delete timers, unregister memory regions, and disconnect interrupts.

3. **Reference Counting**:
   - Pairing `object_ref()` and `object_unref()`.
   - Child objects attached via `object_property_add_child()` have their ownership managed by the parent. Ensure objects are unparented (`object_unparent()`) when removed.
