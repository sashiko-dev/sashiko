+++
title = "Board and SoC Porting"
paths = [
    "boards/",
    "soc/",
]
+++

# Board and SoC Porting

Applies to board and SoC definitions under `boards/` and `soc/`.

## Board YAML and Twister metadata

Source: doc/hardware/porting/board_porting.rst; doc/develop/twister/index.rst.

- Keep `supported:` in `<board>.yaml` populated with the hardware features
  enabled on the board using valid Zephyr board tags (check sibling board YAML
  files in `boards/` with `git_grep`, for example `netif:eth` rather than
  ad-hoc names such as `ethernet`). Leaving `supported:` empty renders an empty
  hardware table in `.. zephyr:board-supported-hw::`.
- Entries in `testing: ignore_tags:` have no effect unless matching tags appear
  in `supported:` or in tests targeting the board. Flag dead `ignore_tags`
  entries at `Low` severity.

## Board Devicetree factoring and documentation

Source: doc/hardware/porting/board_porting.rst.

- Factor shared peripherals, pinmux, and memory layout across sibling boards or
  SoC variants into a common `.dtsi` instead of duplicating full `.dts` files.
- Board directory name, vendor prefix, `board.yml`, Devicetree, and board
  documentation text must agree on the board identity, SoC, and vendor.
- Reuse vendor common `.rst` includes where sibling boards do so, and do not add
  a manual `.. image::` directive in `doc/index.rst` when `doc/img/<board>.webp`
  is auto-detected by the board catalog generator (`git_grep` sibling boards to
  confirm the convention).

## Flash and debug runners (`board.cmake`)

Source: doc/hardware/porting/board_porting.rst (Flash and debug support).

- Do not hardcode OS-specific device paths (such as a fixed `/dev/ttyUSB0`) or
  assume local udev rules in `board.cmake`; keep device selection overridable so
  multiple boards and non-Linux hosts work.
- Remember that the first `include()` in `board.cmake` sets the default flash or
  debug runner if not already set; verify runner ordering matches the board's
  documented default runner.
