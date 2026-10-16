+++
title = "GPIO"
paths = ["drivers/gpio/", "include/linux/gpio/", "include/linux/gpio.h"]
symbols = ["gpio_chip", "gpiochip_*", "gpio_get_value", "gpio_set_value", "gpiod_get_raw_value", "gpiod_set_raw_value", "GPIO_GENERIC", "GPIO_REGMAP"]
+++
# GPIO: maintainer notes

Maintainer preferences for GPIO drivers, consumers and related device tree changes.

- Do not accept new code that includes the legacy <linux/gpio.h> header; drivers should use <linux/gpio/driver.h> and consumers <linux/gpio/consumer.h>.
- When converting legacy gpio_get_value()/gpio_get_value_cansleep() users to the gpiod_* descriptor API changes line polarity semantics, recommend adding a polarity quirk for the binding in of_gpio_try_fixup_polarity() (drivers/gpio/gpiolib-of.c) instead of working around it.
- Do not recommend raw accessors such as gpiod_get_raw_value()/gpiod_get_raw_value_cansleep() as a workaround for polarity issues; they are reserved for especially peculiar fringe use cases.
- For new drivers whose hardware has regular 8/16/32/64-bit registers with one bit per GPIO line, recommend using the generic MMIO GPIO library (select GPIO_GENERIC, <linux/gpio/generic.h>), grouping lines into banks with one GPIO chip per bank where needed.
- In device tree review, suggest splitting a single node exposing many GPIOs (e.g. 128) into one node per register bank (e.g. 4 x 32) so the hardware maps naturally onto GPIO_GENERIC.
- For new drivers that already use regmap, suggest the GPIO regmap helper library (select GPIO_REGMAP, <linux/gpio/regmap.h>) when registers and bitmasks follow a repeating pattern.
