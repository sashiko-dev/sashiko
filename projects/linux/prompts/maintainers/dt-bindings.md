+++
title = "DT Bindings"
paths = ["Documentation/devicetree/bindings/"]
symbols = []
+++
# DT Bindings: maintainer notes

Conventions for structuring Device Tree binding YAML schemas under Documentation/devicetree/bindings/.

- If per-compatible if-then conditionals in a single binding schema become unwieldy because of variant differences, prefer splitting the binding into separate schema documents rather than piling on more conditionals.
- Check the order of properties in both the schema (the properties: and required: lists) and the DTS node under examples:; all must use the canonical sequence used across the tree — compatible, reg, clocks, clock-names, interrupts, interrupt-names, power-domains, power-domain-names (listing only the properties present); interrupts go after clocks and power-domains go after interrupts, so flag a schema or example node that lists interrupts before clocks.
- When a property has a fixed set of entries (such as a two-entry clocks), name each entry with an items list carrying a per-entry description instead of a free-form description enumerating them, and do not restate schema constraints such as counts or minItems/maxItems in description text.
- Check the patch subject for the redundant word "binding": the dt-bindings: prefix already states the patch adds a binding, so flag subjects that repeat it, e.g. "dt-bindings: usb: Add Vendor SoC DWC3 controller binding" should be "dt-bindings: usb: Add Vendor SoC DWC3 controller".
