# I2C Subsystem Details

This document provides a knowledge reference for reviewing code in the I2C
subsystem.

## Initialization and Data Structures

*   **`struct i2c_device_id`**: Initialized arrays of type `struct i2c_device_id`
    must be declared const and use named initializers.

## Transfer Buffers and DMA Safety

Passing stack-allocated or unaligned buffers to DMA-safe I2C transfers causes memory
corruption and cacheline sharing bugs when I2C bus controllers attempt direct
DMA mapping. Conversely, flagging stack-allocated buffers passed to ordinary I2C
transfer functions as bugs is a false positive that causes unnecessary driver
churn.

*   **Ordinary Transfers (`i2c_transfer`, `i2c_master_send`, `i2c_master_recv`,
    `i2c_smbus_*`)**: Safe to use with stack-allocated, embedded, or vmalloc
    buffers. If an I2C adapter driver requires DMA, it calls
    `i2c_get_dma_safe_msg_buf()`, which checks for `I2C_M_DMA_SAFE`. If absent,
    the core automatically allocates a temporary bounce buffer (`kzalloc` or
    `kmemdup`), performs the transfer, copies read data back, and frees the
    buffer (`i2c_put_dma_safe_msg_buf()`).
*   **DMA-Safe Transfers (`I2C_M_DMA_SAFE`)**: When `I2C_M_DMA_SAFE` is set in
    `struct i2c_msg.flags`, the caller asserts that `msg.buf` is already DMA-safe
    (allocated via `kmalloc`/`kzalloc`, cacheline aligned, not on stack, not in
    vmalloc). This allows `i2c_get_dma_safe_msg_buf()` to return the buffer directly
    without allocating a bounce buffer.
*   **False Positive Warning**: Do NOT flag stack-allocated buffers passed to
    ordinary I2C transfer functions (`i2c_master_send()`, `i2c_master_recv()`,
    `i2c_transfer()` without `I2C_M_DMA_SAFE`) as bugs. Unlike SPI or USB (where
    stack buffers are illegal for DMA), the I2C core provides automatic bounce
    buffering when needed.
*   **REPORT as bugs**: Any code that passes a stack-allocated, vmalloced, or
    non-cacheline-aligned buffer in an `i2c_msg` that sets the `I2C_M_DMA_SAFE` flag.

```c
// CORRECT: Using stack buffer with ordinary transfer (core handles bounce if needed)
int read_reg_ordinary(struct i2c_client *client, u8 reg, u8 *val)
{
	u8 buf[2] = { reg };
	int ret;

	ret = i2c_master_send(client, buf, 1);
	if (ret < 0)
		return ret;
	...
}

// CORRECT: Using heap-allocated buffer with I2C_M_DMA_SAFE to avoid bounce
int read_data_dmasafe(struct i2c_client *client, u8 *dma_buf, int len)
{
	struct i2c_msg msg = {
		.addr = client->addr,
		.flags = client->flags | I2C_M_DMA_SAFE | I2C_M_RD,
		.len = len,
		.buf = dma_buf, /* Allocated via kmalloc(), cacheline aligned */
	};

	return i2c_transfer(client->adapter, &msg, 1);
}

// WRONG: Passing stack buffer with I2C_M_DMA_SAFE (bypasses bounce, breaks DMA)
int read_reg_wrong(struct i2c_client *client, u8 reg)
{
	u8 buf[1] = { reg };
	struct i2c_msg msg = {
		.addr = client->addr,
		.flags = client->flags | I2C_M_DMA_SAFE,
		.len = 1,
		.buf = buf, /* BUG: stack buffer with I2C_M_DMA_SAFE */
	};

	return i2c_transfer(client->adapter, &msg, 1);
}
```

## Adapter DMA Bounce Buffering Contracts (`struct i2c_algorithm`)

Failing to use DMA-safe buffer helpers in I2C adapter drivers that implement DMA
transfers leads to DMA mapping failures or memory corruption when client drivers
pass stack or vmalloc buffers in ordinary I2C messages.

*   **Adapter DMA Requirements**: I2C controller drivers that use DMA must call
    `i2c_get_dma_safe_msg_buf(msg, threshold)` before mapping the buffer for
    DMA. The `threshold` parameter specifies the minimum message length where
    DMA overhead is worthwhile (messages below threshold return `NULL`,
    signaling the driver to fall back to PIO).
*   **Buffer Release**: After the transaction completes (or fails), adapter
    drivers must call `i2c_put_dma_safe_msg_buf(buf, msg, xferred)` to copy read
    data back to the client's original buffer and free any allocated bounce
    buffer.

See `i2c_get_dma_safe_msg_buf()` and `i2c_put_dma_safe_msg_buf()` in
`drivers/i2c/i2c-core-base.c`.

## Quick Checks

*   **Stack Buffers in Client Drivers**: Verify that stack-allocated buffers are
    only passed to ordinary transfer APIs (`i2c_transfer` without `I2C_M_DMA_SAFE`,
    `i2c_master_send`, etc.) and never with `I2C_M_DMA_SAFE`.
*   **DMA-Safe Buffer Eligibility**: Verify that buffers passed with
    `I2C_M_DMA_SAFE` are allocated via `kmalloc`/`kzalloc` and are not on the
    stack or in vmalloc memory.
*   **Adapter DMA Bounce Buffering**: In I2C controller drivers implementing DMA
    in `master_xfer`, verify that `i2c_get_dma_safe_msg_buf()` and
    `i2c_put_dma_safe_msg_buf()` are properly paired across all success and
    error paths.
