+++
title = "DT Bindings"
paths = ["Documentation/devicetree/bindings/"]
symbols = []
+++
# DT Bindings: maintainer notes

Conventions for structuring Device Tree binding YAML schemas under Documentation/devicetree/bindings/.

- For new bindings or major schema restructurings, if per-compatible `if`/`then` conditionals in a single binding schema become unwieldy because of variant differences, prefer splitting the binding into separate schema documents rather than piling on more conditionals. Do not ask bug-fix commits (`Fixes:`) or small single-property or single-compatible additions to split an existing multi-SoC binding YAML file.
- Do not invent `dtschema` restrictions that `dt_binding_check` does not enforce: there is no `-bytes` standard unit suffix in `property-units.yaml`; single-item `enum:` lists (for example under `contains:`) and `description:` keys inside `if:`/`then:` blocks are valid `dtschema`; and `examples:` only need to illustrate one valid node rather than enumerating every possible IRQ flag or variant.
- When converting legacy free-form `.txt` bindings to DT schema YAML, matching actual in-tree `.dts`/`.dtsi` users rather than copying unused or imprecise compatibles from the old `.txt` file is expected.
