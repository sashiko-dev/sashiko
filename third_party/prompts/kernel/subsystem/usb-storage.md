# USB Storage Subsystem Details

## unusual_devs.h Entry Conventions

Specifying unnecessary subclass or protocol overrides in `UNUSUAL_DEV()` entries
causes the driver to emit a `dev_notice` on every device insertion, asking users
to report the unneeded entry (see `get_device_info()` in
`drivers/usb/storage/usb.c`). This creates persistent log noise for end users.

The `UNUSUAL_DEV()` macro in `drivers/usb/storage/unusual_devs.h` takes ten
positional parameters. The subclass and protocol positions are the seventh and
eighth arguments:

```c
UNUSUAL_DEV(idVendor, idProduct, bcdDeviceMin, bcdDeviceMax,
            vendor_name, product_name,
            use_protocol,   /* subclass: USB_SC_* value */
            use_transport,  /* protocol: USB_PR_* value */
            init_function, Flags)
```

Note: the kernel's field names are historically confusing. The `useProtocol`
field in `struct us_unusual_dev` (`drivers/usb/storage/usb.h`) holds a
`USB_SC_*` subclass code, and `useTransport` holds a `USB_PR_*` protocol code.

**Parameter semantics:**

| Value | Meaning |
|---|---|
| `USB_SC_DEVICE` (0xff) | Use the device's self-reported `bInterfaceSubClass` |
| `USB_PR_DEVICE` (0xff) | Use the device's self-reported `bInterfaceProtocol` |
| Specific `USB_SC_*` value | Override the device's subclass (e.g., `USB_SC_SCSI` = 0x06) |
| Specific `USB_PR_*` value | Override the device's protocol (e.g., `USB_PR_BULK` = 0x50) |

When an explicit override matches what the device already reports,
`get_device_info()` detects the redundancy and logs a notice -- unless the
entry carries `US_FL_NEED_OVERRIDE` (defined in `include/linux/usb_usual.h`),
which suppresses the warning for entries where the override is intentionally
required.

**Entry statistics:**

Roughly 85% of `UNUSUAL_DEV()` entries in `unusual_devs.h` use
`USB_SC_DEVICE, USB_PR_DEVICE`. Entries with explicit overrides are the
minority and typically exist because the device mis-reports its subclass or
protocol, or because the entry needs a non-standard transport handler.

**Cross-referencing device descriptors:**

Device descriptor output from `/sys/kernel/debug/usb/devices` shows the
device's self-reported interface values:

```
I:* If#= 0 Alt= 0 #EPs= 2 Cls=08(stor.) Sub=06 Prot=50 Driver=usb-storage
```

Here `Sub=06` corresponds to `USB_SC_SCSI` and `Prot=50` corresponds to
`USB_PR_BULK`. If the `UNUSUAL_DEV()` entry specifies these same values
explicitly rather than using `USB_SC_DEVICE` / `USB_PR_DEVICE`, the overrides
are redundant and the driver will emit the unnecessary-override notice.

```c
// CORRECT: Let the driver use the device's self-reported values
UNUSUAL_DEV(0x1234, 0x5678, 0x0100, 0x0100,
    "Vendor", "Product",
    USB_SC_DEVICE, USB_PR_DEVICE, NULL,
    US_FL_NO_ATA_1X)

// WRONG: Unnecessary override when device already reports Sub=06 Prot=50
UNUSUAL_DEV(0x1234, 0x5678, 0x0100, 0x0100,
    "Vendor", "Product",
    USB_SC_SCSI, USB_PR_BULK, NULL,
    US_FL_NO_ATA_1X)
```

## Quick Checks

- When a patch adds an `UNUSUAL_DEV()` entry with explicit `USB_SC_*` /
  `USB_PR_*` values instead of `USB_SC_DEVICE` / `USB_PR_DEVICE`, the commit
  message should explain why the override is needed.
- If the commit includes device descriptor output, compare the `Sub=` and
  `Prot=` fields against the entry's seventh and eighth arguments to detect
  redundant overrides.
