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

## `select`, `depends on`, and circular dependencies

Source: doc/build/kconfig/tips.rst (What to turn into Kconfig options, select
statements, select pitfalls, Using select for helper symbols, select
recommendations).

- Avoid selecting symbols that have prompts or dependencies; prefer `depends on`.
  Whenever a symbol's dependencies change, the dependencies of every symbol that
  selects it, directly or indirectly, have to change too. Check them when a change
  adds a `select` or adds a dependency to a selected symbol.
- Remember that `select FOO` (including `select FOO if COND`) forces `FOO=y`
  whenever the selecting condition holds and overrides user `=n` assignments. Do
  not describe a `select` in help text or comments as a default that users can
  turn off; use `default y if COND` (or `imply` when weak default semantics are
  intended) when users must remain able to disable `FOO`.
- Selecting simple helper symbols without prompts or dependencies is fine. When
  symbol `B` is `select`ed by `A` and is not meant to be enabled standalone by
  users, make `B` promptless (`bool` without a prompt string) instead of adding
  `depends on A` to `B`; adding `depends on A` creates a Kconfig circular
  dependency (`A` selects `B`, `B` depends on `A`).
- Buses are an exemption: drivers on I2C, SPI, MFD, PCIe, and similar buses use
  `select` so that the bus driver is activated with the device.
- Do not build chains of symbols selecting each other, except simple helper
  symbols.
- `depends on` also applies to `string`, `int`, and `hex` symbols. Add it so that
  an option does not appear when its feature is disabled, and gate capability-
  specific options with `depends on MMU` (or the required architecture or kernel
  capability).

## Defaults set outside the base definition (`configdefault` and `Kconfig.defconfig`)

Source: doc/build/kconfig/setting.rst (Multiple symbol definitions);
doc/build/kconfig/extensions.rst (configdefault);
doc/build/kconfig/tips.rst ((Lack of) conditional includes);
doc/hardware/porting/board_porting.rst.

- Each definition of a symbol is independent, and the symbol can be `y` if the
  conditions of any definition hold. A plain `config FOO` with a `default` in a
  `Kconfig.defconfig` file can therefore weaken the dependencies of the base
  definition. Include enclosing `if` and `menu` dependencies and the conditions
  inherited through `source` when comparing definitions. Prefer
  `configdefault FOO` whenever only adding or overriding a default (remembering
  `configdefault` accepts only `default` entries and enclosing `if` blocks), or
  retain the effective dependencies. Report a dropped dependency only after
  naming a configuration that satisfies the complete new definition and reaches
  a concrete failure; apply the CI exclusion in `false-positive-guide.md` when
  that failure is caught at build time.
- Overriding defaults of existing symbols (especially `menuconfig` symbols such
  as `NET_L2_ETHERNET`) belongs in `Kconfig.defconfig`, not in a board or SoC
  `Kconfig` file.
- Scope driver or stack-size defaults to the condition that actually requires
  them: gate IP-specific defaults on `DT_HAS_<COMPAT>_ENABLED` and pointer-width
  stack minimums on the architecture or word-size symbol (for example `ARM64` or
  `64BIT`) rather than a single SoC symbol.
- For a changed default, trace the first satisfied default and any explicit
  assignment in the affected board or application configuration. Read its DTS
  and overlays before claiming that a timer or device disappears. A deliberate
  policy change alone is not a defect; identify an affected configuration and
  the behavior it loses.

## Enabling drivers from Devicetree and board `*_defconfig` hygiene

Source: doc/build/dts/dt-vs-kconfig.rst;
doc/hardware/porting/board_porting.rst (General recommendations).

- A driver option is normally `default y` and `depends on
  DT_HAS_<COMPAT>_ENABLED`, so it builds when a matching node has
  `status = "okay"`. Flag a new driver option that omits the dependency.
- Keep board `*_defconfig` files minimal: do not explicitly enable drivers that
  already default to `y` via `DT_HAS_<COMPAT>_ENABLED`, bus symbols already
  `select`ed by enabled drivers (such as `CONFIG_PCIE`), or optional subsystems
  and modes (such as `CONFIG_SMP=y` on boards whose cores can run AMP) unless
  required for basic board bringup (`Low` severity).

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
