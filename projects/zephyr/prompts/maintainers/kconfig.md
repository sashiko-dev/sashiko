+++
title = "Kconfig"
paths = [
    "Kconfig*",
    "**/Kconfig*",
    "*.conf",
    "**/*.conf",
    "*_defconfig",
    "**/*_defconfig",
]
+++

# Kconfig

Applies to Kconfig files and to `.conf` and `_defconfig` files. CI checks that
symbols exist and are formatted correctly, so review the logic instead.

## What belongs in Kconfig

Source: doc/build/kconfig/tips.rst (What not to turn into Kconfig options);
doc/build/dts/dt-vs-kconfig.rst.

- Do not add options that name a device, or that specify fixed hardware
  configuration such as a GPIO pin. These belong in Devicetree.
- Only give a symbol a prompt if it makes sense for a user to change it. Otherwise
  it should be hidden and derive its value from other symbols.

## `select` and `depends on`

Source: doc/build/kconfig/tips.rst (select pitfalls, select recommendations).

- Avoid selecting symbols that have prompts or dependencies; prefer `depends on`.
  Whenever a symbol's dependencies change, the dependencies of every symbol that
  selects it, directly or indirectly, have to change too. Check them when a change
  adds a `select` or adds a dependency to a selected symbol.
- Selecting simple helper symbols without prompts or dependencies is fine.
- Buses are an exemption: drivers on I2C, SPI, MFD, and similar buses use `select`
  so that the bus driver is activated with the device.
- Do not build chains of symbols selecting each other, except simple helper
  symbols.
- `depends on` also applies to `string`, `int`, and `hex` symbols. Add it so that
  an option does not appear when its feature is disabled.

## Defaults set outside the base definition

Source: doc/build/kconfig/setting.rst (Multiple symbol definitions);
doc/build/kconfig/extensions.rst (configdefault);
doc/build/kconfig/tips.rst ((Lack of) conditional includes).

- Each definition of a symbol is independent, and the symbol can be `y` if the
  conditions of any definition hold. A plain `config FOO` with a `default` in a
  `Kconfig.defconfig` file can therefore weaken the dependencies of the base
  definition. Include enclosing `if` and `menu` dependencies and the conditions
  inherited through `source` when comparing definitions. Use `configdefault FOO`,
  or retain the effective dependencies. Report a dropped dependency only after
  naming a configuration that satisfies the complete new definition and reaches
  a concrete failure; apply the CI exclusion in `false-positive-guide.md` when
  that failure is caught at build time.
- For a changed default, trace the first satisfied default and any explicit
  assignment in the affected board or application configuration. Read its DTS
  and overlays before claiming that a timer or device disappears. A deliberate
  policy change alone is not a defect; identify an affected configuration and
  the behavior it loses.

## Enabling drivers from Devicetree

Source: doc/build/dts/dt-vs-kconfig.rst.

- A driver option is normally `default y` and `depends on
  DT_HAS_<COMPAT>_ENABLED`, so it builds when a matching node has
  `status = "okay"`. Flag a new driver option that omits the dependency.

## Configuration files

Source: doc/build/kconfig/tips.rst (Assignments to promptless symbols).

- Assignments to hidden symbols in configuration files are always ignored. Flag a
  `.conf` assigning a promptless symbol, since the setting silently has no effect.

## Code that branches on configuration

Source: doc/contribute/coding_guidelines/index.rst (Rule A.1).

- Prefer `if (IS_ENABLED(CONFIG_FOO))` over a preprocessor conditional when the
  code path changes with an option, so both paths stay compiled. Do not report
  existing `#ifdef` uses that the change does not touch.
- Trace the disabled path: a call under a condition must still link and behave
  correctly when the option is off, and a field conditionally present in a
  structure must not be used outside the same condition.
