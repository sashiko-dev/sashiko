+++
title = "Netlink"
paths = ["Documentation/netlink/specs/"]
symbols = ["genl_*", "nla_*", "NLA_*", "NLM_F_*", "nlmsg_*", "netlink_callback"]
+++
# Netlink: maintainer notes

Netlink and Generic Netlink uAPI design conventions for new or changed families, commands, attributes, extended ACK use and YAML specs.

- Netlink message layout becomes uAPI once it reaches user space and cannot be changed later, so design mistakes are treated as unfixable after the first release.
- The new-family design rules below apply only to new families; if a rule is already broken within an existing family, do not demand it in future extensions of that family.
- New families should start the first attribute and first command ID at 1 and avoid an "unspec" (value 0) attribute or command.
- Use the same command ID for the request and reply of an operation, and a separate command ID for each notification.
- New families must use the "unified" message-ID model; the "directional" model is for legacy families only.
- A "do" operation should never reply with multiple messages (NLM_F_MULTI); use a filtered dump instead.
- kernel-policy should be "per-op" (the default) or "split"; do not use "global" for new families.
- Do not introduce new uses of the request-type-specific flags (NLM_F_REPLACE, NLM_F_EXCL, NLM_F_CREATE, NLM_F_APPEND, NLM_F_NONREC, NLM_F_BULK, NLM_F_ATOMIC, NLM_F_ROOT, NLM_F_MATCH); they are deprecated outside the legacy families that already use them.
- All operations, especially NEW/ADD, should reply with a full message carrying identifying information about the object (e.g. an allocated ID); err on the side of replying rather than only ACKing, and do not rely on NLM_F_ECHO to convey created-object info.
- Provide extended ACK information on errors and, where useful, on success (as warnings).
- Prefer the standard extack attributes (NL_SET_BAD_ATTR, NL_SET_ERR_ATTR_MISS) over plain text messages; omit the text message when the attribute info plus errno already explain the problem.
- Prefer repeated (multi-attr) attributes for arrays; no extra wrapping nest, no indexed-array and no type-value encodings in new families.
- Avoid binary structures inside attributes; break each member into its own attribute.
- Prefer variable-width uint/sint over fixed-width integer types in most cases.
- Avoid integer types smaller than 32 bits unless the value is genuinely u8/u16 (e.g. a protocol header field).
- Strings should be NUL-terminated by default; set unterminated-ok only for legacy families.
- New Generic Netlink families must reject unknown attributes.
- Declaring the right attribute validation policy (an NLA_POLICY_* helper) is strongly preferred over open-coded validation.
- In YAML specs, define an attribute's value only in its main set, never in a subset-of set.
- YAML spec license must be ((GPL-2.0 WITH Linux-syscall-note) OR BSD-3-Clause).
- YAML specs must be self-contained, with no dependency on other specs or C headers except via the header property for shared constants (e.g. IFNAMSIZ).
- Each major property in a YAML spec should carry a doc.
- Prefer notify: (sharing contents with a GET op) over event:; events are less idiomatic unless the information is something users would never query directly.
- Names in YAML specs should use dashes, not underscores.
