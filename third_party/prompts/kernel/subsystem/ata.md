# ATA Subsystem Details

## Device Validation and Compatibility

Strict validation checks in device initialization, log reading, or capability
detection paths can break compatibility with previously-working devices,
causing features to be incorrectly disabled or devices to fail initialization.
ATA devices frequently deviate from ACS specifications in benign ways that do
not affect functionality.

**ATA specification compliance reality:**

ATA devices commonly deviate from specifications without functional impact:
- Version fields reporting `0x0000` instead of spec-defined values (e.g.,
  General Purpose Log Directory version, where `0x0001` is expected)
- Reserved bits not being zero
- Slightly different formatting in log pages
- Missing or zero-filled optional fields

**Validation strictness principles:**

When reviewing patches that add new validation to device initialization paths
(e.g., `ata_dev_configure()`, `ata_read_log_directory()`, log page parsing):

- **Strict validation requires justification**: New checks that could reject
  previously-working devices need explicit discussion of why strict enforcement
  is necessary and what devices might be affected
- **Warn before failing**: For specification compliance checks that do not
  affect data integrity, the safer pattern is to warn (using `ata_dev_warn()`
  or `ata_dev_warn_once()`) rather than return an error
- **Avoid permanently disabling features**: Setting quirks programmatically
  (e.g., `ATA_QUIRK_NO_LOG_DIR`) or clearing cached data via
  `ata_clear_log_directory()` in response to validation failures can disable
  features for devices that would otherwise work

**When to fail strictly vs warn:**

| Condition | Action |
|-----------|--------|
| Data integrity issues (invalid checksums, corrupted structures) | Fail with error |
| I/O errors reading device data | Fail with error |
| Safety-critical features (NCQ, TRIM, security) with invalid config | Fail with error |
| Version mismatch on otherwise-valid structure | Warn and continue |
| Optional fields not formatted per spec | Warn and continue |
| Reserved bits non-zero | Ignore or warn |

```c
// WRONG: Strict version check that disables features with no fallback
if (version != EXPECTED_VERSION) {
    ata_dev_err(dev, "Invalid version 0x%04x", version);
    ata_clear_log_directory(dev);
    dev->quirks |= ATA_QUIRK_NO_LOG_DIR;
    return -EINVAL;
}
```

```c
// CORRECT: Warn about non-compliance but continue
if (version != EXPECTED_VERSION)
    ata_dev_warn_once(dev, "Unexpected version 0x%04x", version);
// Continue using the data if it appears valid
```

## Quick Checks

- **New validation in init paths**: When a patch adds validation checks to
  `ata_dev_configure()`, `ata_read_log_directory()`, or similar functions,
  verify the commit message justifies strict enforcement and considers
  compatibility with existing devices
- **Programmatic quirk setting**: Code that sets `ATA_QUIRK_*` flags based on
  runtime validation (not the static quirk table in `__ata_dev_quirks`) should
  have a fallback or recovery mechanism
- **Error vs warning for spec deviations**: Distinguish between functional
  errors (I/O failures, corruption) and cosmetic spec violations (version
  mismatches, formatting differences)
