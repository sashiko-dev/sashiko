# Device Tree Bindings Subsystem Details

## Compatible String Conditional Blocks

Adding a new compatible string to a YAML binding schema without updating all
existing `if-then` conditional blocks causes schema validation to be
incomplete. Device trees with invalid configurations (wrong number of
interrupts, clocks, or required properties) will silently pass validation.

YAML binding schemas use `allOf` with `if-then` blocks to apply different
constraints based on the compatible string (see
`Documentation/devicetree/bindings/example-schema.yaml` and
`Documentation/devicetree/bindings/writing-schema.rst`). The `if` clause
typically matches using `properties:compatible:contains:enum:` or
`properties:compatible:contains:const:`. When a new compatible string follows
a generational pattern (e.g., `vendor,device-gen5` added alongside existing
`vendor,device-gen2/gen3/gen4`), every `if` block that enumerates prior
generations must include the new string if the hardware shares the same
constraints.

Properties commonly guarded by these conditional blocks:

| Property | Typical constraint |
|----------|-------------------|
| `interrupts` | `maxItems`, `minItems` |
| `clocks` | Number and order of required clocks |
| `resets` | Reset line count |
| `power-domains` | Power domain count |
| `required` | Which properties must be present |
| `reg` | Number and meaning of register regions |

A new compatible string that is added to the top-level `compatible` definition
but omitted from existing `if:properties:compatible:contains:enum:` blocks
where previous generations appear is a bug when the hardware shares the same
constraints as those previous generations.

## Hardware Variant Required Properties

Adding a compatible string for a hardware variant with additional capabilities
(GPIO controller, PWM output, clock provider, interrupt controller) without
documenting the corresponding required properties allows incomplete device
tree nodes to pass schema validation. At runtime, drivers or dependent
subsystems will fail when they attempt to use the undocumented functionality.

When hardware gains new provider capabilities, the binding must add the
corresponding standard properties to the `required` list and define their
constraints:

| Capability | Required properties |
|------------|---------------------|
| GPIO controller | `gpio-controller`, `#gpio-cells` |
| PWM output | `#pwm-cells` |
| Clock provider | `#clock-cells` |
| Interrupt controller | `interrupt-controller`, `#interrupt-cells` |
| Reset provider | `#reset-cells` |

Each cell-count property must have a `const` constraint matching the
hardware (e.g., `#gpio-cells: const: 2`). The `examples` section must
include all required properties to pass `dt_binding_check`.

If the conditionals in a single YAML file become unwieldy due to variant
differences, the binding should be split into a separate YAML file (see the
comment in `Documentation/devicetree/bindings/example-schema.yaml` at the
`allOf` section: "If the conditionals become too unwieldy, then it may be
better to just split the binding into separate schema documents").

## `$id` Path Consistency

An incorrect `$id` field breaks the schema cross-reference system. Schema
references (`$ref`) from other bindings will not resolve, and
`dt_binding_check` (the `make` target defined in
`Documentation/devicetree/bindings/Makefile`) may report misleading errors
or silently skip validation.

The `$id` field must begin with `http://devicetree.org/schemas/` (see
`Documentation/devicetree/bindings/writing-schema.rst`). The path after
that prefix must exactly match the file path relative to
`Documentation/devicetree/bindings/`.

```yaml
# File: Documentation/devicetree/bindings/gpio/vendor,device.yaml

# CORRECT
$id: http://devicetree.org/schemas/gpio/vendor,device.yaml#

# WRONG - missing subdirectory component
$id: http://devicetree.org/schemas/vendor,device.yaml#
```

Common causes of mismatch:
- Missing subdirectory path component (e.g., omitting `gpio/`, `power/`,
  `clock/`)
- Stale filename from a `.txt` to `.yaml` conversion
- Copy-paste from another binding without updating the path

## Quick Checks

- When a compatible string with a generation marker (gen5, v5, series-5,
  etc.) is added, all `if` blocks referencing prior generations must be
  checked for the new string
- When a binding has multiple YAML files for different device types in the
  same family, related files may need matching updates
- When a variant compatible string adds provider capabilities (GPIO, PWM,
  clock, interrupt, reset), the corresponding properties must appear in
  the `required` list with appropriate `const` constraints
- The `$id` path must match the file location; this is especially error-prone
  during `.txt` to `.yaml` conversions
