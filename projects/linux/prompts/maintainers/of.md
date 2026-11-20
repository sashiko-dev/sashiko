+++
title = "Open Firmware and Devicetree API"
paths = []
symbols = ["of_alias_get_id", "of_alias_get_highest_id", "of_node_name_*", "of_node_full_name", "of_find_node_by_name", "of_find_node_by_path" ]
+++
# Open Firmware and Devicetree: maintainer notes

- Every usage of OF aliases (of_alias_get_id(), of_alias_get_highest_id()) needs to be documented as ABI in Devicetree bindings. This is usually a textual documentation, not schema.
- Node name of device nodes should not be treated as an ABI by individual drivers, except for device child nodes (e.g. "ports") when defined in Devicetree bindings. Linux calls like of_node_name_eq(), of_node_full_name() and others can only be used for a device child nodes and requires to be documented as ABI in Devicetree bindings.
