+++
title = "KHO (Kexec Handover)"
paths = ["lib/test_kho.c", "kernel/liveupdate/kexec_handover.c"]
symbols = ["kho_*", "kho_is_enabled", "kho_retrieve_subtree", "kho_preserve_folio", "kho_add_subtree", "register_kho_notifier"]
+++
# KHO (Kexec Handover): maintainer notes

This covers design conventions of the Kexec Handover subsystem that reviewers should respect.

- KHO FDTs deliberately store all values in native host endianness rather than the big-endian format required by the devicetree specification, because they are only consumed by a successor kernel on the same machine; do not flag native-endian reads or writes of KHO FDT properties as bugs.
