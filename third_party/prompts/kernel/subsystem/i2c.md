# I2C Client Details

## API

- debugfs entries attached to the `debugfs` object in `struct i2c_client` are
  cleaned up by the I2C subsystem core in the client device removal function
  after calling the client driver remove function and before releasing client
  device resources allocated with devres functions, and in the I2C subsystem
  probe function after a probe failure has been reported by the driver's probe
  function.
