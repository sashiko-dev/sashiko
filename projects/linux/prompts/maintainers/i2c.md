+++
title = "I2C"
paths = ["drivers/i2c/", "include/linux/i2c.h"]
symbols = ["i2c_*", "i2c_transfer", "i2c_master_send", "i2c_master_recv", "i2c_smbus_*", "i2c_get_dma_safe_msg_buf"]
+++
# I2C: maintainer notes

Coding conventions the I2C community expects in drivers using the I2C subsystem.

- Initialized arrays of struct i2c_device_id should be declared const.
- New or changed struct i2c_device_id entries should use named initializers.
- Do not flag positional initializers in existing struct i2c_device_id tables as bugs; only new or changed entries are expected to use named initializers.
