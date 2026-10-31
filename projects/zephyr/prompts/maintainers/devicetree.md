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

## Rules for upstream bindings

Source: doc/build/dts/bindings-upstream.rst.

- Compare with an existing Linux or vendor binding when it is available in the
  tree or supplied context. Check that its properties are reused and divergences
  are explained. Leave unavailable external bindings unassessed.
- A binding for compatible `vnd,foo` is named `vnd,foo.yaml`; a bus-specific
  binding may add the bus to the name.
- A device with a vendor listed in `dts/bindings/vendor-prefixes.txt` must use that
  prefix. A device not made by a specific hardware vendor must not get one.
- Where `default:` is used, the `description:` must explain why that value was
  chosen and when a different one is needed. If the right value depends on the
  board, the property should be `required: true` instead.
- Use the `zephyr,` prefix for Zephyr-specific extensions to bindings shared with
  Linux, and for configuration specific to a Zephyr driver.
