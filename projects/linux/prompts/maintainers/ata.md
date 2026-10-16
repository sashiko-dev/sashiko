+++
title = "ATA/libata"
paths = ["drivers/ata/"]
symbols = ["ata_dev_*", "ata_port_*", "ata_read_log_*", "ATA_QUIRK_*"]
+++
# ATA/libata: maintainer notes

These notes cover the ATA/libata community's policy on device compatibility when patches add new validation to device initialization, log reading or capability detection.

- Compatibility with devices that already work takes priority over strict ACS specification compliance. Benign spec deviations should not cause a device to fail or lose features.
- A new check that could reject devices that used to work needs justification. The commit message should explain why strict enforcement is necessary and which devices might be affected.
- For spec-compliance checks that do not affect data integrity, warn (for example with ata_dev_warn() or ata_dev_warn_once()) and continue instead of returning an error.
- Fail hard only for real functional problems such as data integrity issues, I/O errors, or invalid configuration of safety-critical features (NCQ, TRIM, security). Cosmetic deviations such as version mismatches, formatting differences or non-zero reserved bits get a warning or are ignored.
- Avoid permanently disabling features in response to validation failures, for example by setting ATA_QUIRK_* flags at runtime or clearing cached log data. Runtime quirk setting outside the static quirk table should come with a fallback or recovery path.
