+++
title = "DTS (DT sources)"
paths = ["arch/arc/boot/dts/", "arch/arm64/boot/dts/", "arch/arm/boot/dts/", "arch/csky/boot/dts/", "arch/loongarch/boot/dts/", "arch/microblaze/boot/dts/", "arch/mips/boot/dts/", "arch/nios2/boot/dts/", "arch/openrisc/boot/dts/", "arch/powerpc/boot/dts/", "arch/riscv/boot/dts/", "arch/sh/boot/dts/", "arch/xtensa/boot/dts/"]
symbols = []
+++
# Devicetree Sources: maintainer notes

- No custom aliases are allowed in DTS, except a few subsystem generic ones defined in subsystem-level code, like ethernet, gpio, mdio-gpio, mmc, mtd, i2c, i3c, pinctrl, rtc, serial, spi and watchdog.
- Node names should be generic. List of examples (not exhaustive) is in Devicetree specification and below:,
  adc, accelerometer, air-pollution-sensor, atm, audio-codec, audio-controller, backlight, bluetooth, bus, cache-controller, camera, can, charger, clock, clock-controller, co2-sensor, compact-flash, cpu, cpus, crypto, disk, display, dma-controller, dsi, dsp, eeprom, efuse, endpoint, ethernet, ethernet-phy, fdc, flash, gnss, gpio, gpu, gyrometer, hdmi, hwlock, i2c, i2c-mux, ide, interrupt-controller, iommu, isa, keyboard, key, keys, lcd-controller, led, leds, led-controller, light-sensor, lora, magnetometer, mailbox, mdio, memory, memory-controller, mmc, mmc-slot, mouse, nand-controller, nvram, oscillator, parallel, pc-card, pci, pcie, phy, pinctrl, pmic, pmu, port, ports, power-monitor, pwm, regulator, reset-controller, rng, rtc, sata, scsi, serial, sound, spi, sram-controller, ssi-controller, syscon, temperature-sensor, timer, touchscreen, tpm, usb, usb-hub, usb-phy, video-codec, vme, watchdog, wifi.
- Node name of device nodes should not be treated as an ABI by individual drivers, except for device child nodes (e.g. "ports") when defined in Devicetree bindings.
