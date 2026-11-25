+++
title = "Devicetree and Bindings"
paths = [
    "boards/",
    "dts/",
    "*.dts",
    "*.dtsi",
    "*.overlay",
    "**/*.dts",
    "**/*.dtsi",
    "**/*.overlay",
]
symbols = [
    "DEVICE_DT_GET*",
    "DT_ALIAS",
    "DT_CHOSEN",
    "DT_COMPAT_*",
    "DT_DRV_COMPAT",
    "DT_GPIO_*",
    "DT_HAS_*",
    "DT_INST_*",
    "DT_NODELABEL",
    "DT_NODE_*",
    "DT_PROP*",
]
+++

# Devicetree and Bindings

Applies to `.dts`, `.dtsi`, overlay files, binding YAML, and code using `DT_*`
macros. DTS and bindings are not C, so check how the C code consumes them.

## Keeping code and Devicetree in agreement

Source: doc/build/dts/howtos.rst; doc/build/dts/dt-vs-kconfig.rst.

- `DEVICE_DT_GET()` of a node without `status = "okay"` can compile and then fail
  to link with an undefined `__device_dts_ord_N`. Guard optional nodes with
  `DT_NODE_HAS_STATUS()` or use `DEVICE_DT_GET_OR_NULL()`.
- A change to an SoC `.dtsi` affects every board using it. Check boards that
  relied on the old value, particularly a change to `status`, a clock, or
  a `reg` base address.
- Validate paired optional properties symmetrically at build time (for example,
  checking with `BUILD_ASSERT` that `dmas` and `dma-names` are either both
  present or both absent) so half-configured Devicetree nodes fail at build time
  instead of misbehaving at runtime.
- Avoid deprecated `CONFIG_SRAM_BASE_ADDRESS` and `CONFIG_SRAM_SIZE` in new
  code; derive RAM base and size from `DT_CHOSEN(zephyr_sram)` (`DT_REG_ADDR` /
  `DT_REG_SIZE`) and watch for 32-bit overflow when RAM size reaches or exceeds
  4 GiB.

## Rules for upstream bindings

Source: doc/build/dts/bindings-upstream.rst; doc/build/dts/bindings-syntax.rst.

- Compare with an existing Linux or vendor binding when it is available in the
  tree or supplied context. Check that its properties are reused and divergences
  are explained. Leave unavailable external bindings unassessed.
- A binding for compatible `vnd,foo` is named `vnd,foo.yaml`; a bus-specific
  binding may add the bus to the name.
- A device with a vendor listed in `dts/bindings/vendor-prefixes.txt` must use that
  prefix. A device not made by a specific hardware vendor must not get one.
- Give bindings with multi-paragraph `description:` blocks a concise `title:`
  (at most 100 characters, omitting the words "binding", "schema", and
  "driver"). Do not copy or rewrite an inherited property's `description:` from
  an `include:`d binding unless specializing it; remember `include:` forbids
  overriding keys with conflicting values (except strengthening `required: true`
  and unioning `class:`).
- Where `default:` is used, the `description:` must explain why that value was
  chosen and when a different one is needed. If the right value depends on the
  board, the property should be `required: true` instead. Conversely, when a
  property has one hardware-defined fallback value, prefer setting `default:` in
  the binding and reading it with `DT_PROP()`/`DT_INST_PROP()` rather than
  omitting `default:` and repeating `DT_PROP_OR()` with the same constant in
  driver code.
- Use the `zephyr,` prefix for Zephyr-specific extensions to bindings shared with
  Linux, and for configuration specific to a Zephyr driver.
