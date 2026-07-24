# I2C Subsystem Details

This document provides a knowledge reference for reviewing code in the I2C
subsystem, focusing on buffer allocation contracts and DMA safety.

## Transfer Buffers and DMA Safety

Passing stack-allocated or unaligned buffers to DMA-safe I2C APIs causes memory
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
*   **DMA-Safe Transfers (`i2c_master_send_dmasafe`, `i2c_master_recv_dmasafe`,
    `I2C_M_DMA_SAFE`)**: These APIs assert that the buffer passed by the client
    driver is already DMA-safe (allocated via `kmalloc`/`kzalloc`, cacheline
    aligned, not on stack, not in vmalloc). This allows
    `i2c_get_dma_safe_msg_buf()` to return the buffer directly without
    allocating a bounce buffer.
*   **False Positive Warning**: Do NOT flag stack-allocated buffers passed to
    ordinary I2C transfer functions as bugs. Unlike SPI or USB (where stack
    buffers are illegal for DMA), the I2C core provides automatic bounce
    buffering when needed.
*   **REPORT as bugs**: Any code that passes a stack-allocated, vmalloced, or
    non-cacheline-aligned buffer to `i2c_master_send_dmasafe()`,
    `i2c_master_recv_dmasafe()`, or explicitly sets `I2C_M_DMA_SAFE` on an
    `i2c_msg`.

```c
// CORRECT: Using stack buffer with ordinary transfer (core handles bounce)
int read_reg_ordinary(struct i2c_client *client, u8 reg, u8 *val)
{
	u8 buf[2] = { reg };
	int ret;

	ret = i2c_master_send(client, buf, 1);
	if (ret < 0)
		return ret;
	...
}

// CORRECT: Using heap-allocated buffer with dmasafe API to avoid bounce
int read_reg_dmasafe(struct i2c_client *client, u8 *dma_buf, int len)
{
	/* dma_buf was allocated via kmalloc() and is cacheline-aligned */
	return i2c_master_send_dmasafe(client, dma_buf, len);
}

// WRONG: Passing stack buffer to dmasafe API (bypasses bounce, breaks DMA)
int read_reg_wrong(struct i2c_client *client, u8 reg)
{
	u8 buf[1] = { reg };

	return i2c_master_send_dmasafe(client, buf, 1); // BUG!
}
```

See `i2c_master_send_dmasafe()` and `i2c_master_recv_dmasafe()` in
`include/linux/i2c.h`.

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
    only passed to ordinary transfer APIs (`i2c_transfer`, `i2c_master_send`,
    etc.) and never to `*_dmasafe` variants or messages with `I2C_M_DMA_SAFE`.
*   **DMA-Safe API Eligibility**: Verify that buffers passed to
    `i2c_master_send_dmasafe()`, `i2c_master_recv_dmasafe()`, or marked with
    `I2C_M_DMA_SAFE` are allocated via `kmalloc`/`kzalloc` and are not on the
    stack or in vmalloc memory.
*   **Adapter DMA Bounce Buffering**: In I2C controller drivers implementing DMA
    in `master_xfer`, verify that `i2c_get_dma_safe_msg_buf()` and
    `i2c_put_dma_safe_msg_buf()` are properly paired across all success and
    error paths.
