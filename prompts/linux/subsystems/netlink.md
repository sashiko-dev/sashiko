# Netlink / Generic Netlink uAPI Details

Source of these rules: `Documentation/userspace-api/netlink/` and
`Documentation/core-api/netlink.rst`. Apply when reviewing changes that add
or modify Netlink families, commands, attributes, dump callbacks, extended
ACK reporting, or YAML specs under `Documentation/netlink/specs/`.

Once a netlink message reaches user space, its layout is uAPI and cannot be
changed. Most rules below exist because mistakes are *unfixable* after the
first release.

## Design rules for new families

Following rules apply to **new families** only. If the rule is already broken
within the family it should be ignored in future extensions of the family.

- First attribute and first command ID should be `1`. Avoid type `unspec`
  (value `0`) attribute or command
- Use the **same command ID for request and reply** of a given operation.
- Use a **separate command ID for each notification** so user space can route
  notifications independently of replies.
- New families must use the `unified` message-ID model. The `directional`
  model is for legacy families only.
- A `do` operation must **never** reply with multiple messages /
  `NLM_F_MULTI`. Use a filtered dump instead.
- `kernel-policy` should be `per-op` (the default) or `split`. Do **not** use
  `global` for new families - it prevents per-command attribute narrowing.
- Do not introduce new uses of request-type-specific flags
  (`NLM_F_REPLACE`, `NLM_F_EXCL`, `NLM_F_CREATE`, `NLM_F_APPEND`,
  `NLM_F_NONREC`, `NLM_F_BULK`, `NLM_F_ATOMIC`, `NLM_F_ROOT`,
  `NLM_F_MATCH`). They are deprecated outside the legacy families that
  already use them.

## Replies, ACKs and notifications

- All operations, especially `NEW`/`ADD`, **must reply with a full message**
  carrying identifying information about the new object (e.g. allocated ID).
  Once a command only ACKs, that becomes uAPI; err on the side of replying.
  Do not rely on `NLM_F_ECHO` to convey created-object info.
- When emitting a notification in response to a request, **pass the request
  info to `genl_notify()`** so `NLM_F_ECHO` is honored.

## Dumps

- If iteration during a dump may skip or repeat objects (e.g. due to lockless
  data structures), set `NLM_F_DUMP_INTR` on the affected message(s).
  This is normally implemented by maintaining a generation counter and
  recording it in `netlink_callback.seq`.

## Extended ACK

- Provide extended ACK info on errors **and** in the success path
  (treated as warnings) where useful.
- Prefer the standard attributes over plain text message if possible:
  - `NL_SET_BAD_ATTR`      - bad attribute
  - `NL_SET_ERR_ATTR_MISS` - missing attribute
- If the attribute info + returned errno sufficiently explain the problem
  the plain text message should **not** be included.

## Attribute design

For any new nla_get_* / nla_put_* call **always** check if the type agrees
with the YAML spec, validation policy and with other uses.

- **Prefer repeated (`multi-attr`) attributes for arrays** over nested or
  indexed encodings. No extra wrapping nest, no `indexed-array`, no
  `type-value` for new families.
- **Avoid binary structures inside attributes.** Break each member into its
  own attribute. Binary structs hurt validation and extensibility and are
  actively discouraged for new attributes.
- **Prefer `uint`/`sint` (variable-width 64-bit) over fixed-width** integer
  types in most cases.
- **Avoid integer types smaller than 32 bits** - they save no memory due to
  the 4-byte attribute alignment. Unless the value carried is actually
  guaranteed to be u8 or u16 (e.g. it's a protocol header field).
- For 64-bit integers in legacy fixed structs use the `pad` attribute (only
  one per attribute set is allowed).
- Strings default to NUL-terminated (`NLA_NUL_STRING`); only set
  `unterminated-ok` for legacy families. `max-len` does **not** count the
  terminator - specs commonly write `max-len: CONST - 1`.

## Validation policy

- New Generic Netlink families must reject unknown attributes (this is the
  default for new families and for those opting into strict checking).
- Declaring the right attribute validation policy is strongly preferred
  over open coded validation. Find ``NLA_POLICY_*`` that would have worked.

## YAML spec hygiene

When reviewing files under `Documentation/netlink/specs/`:

- An attribute's `value` is defined only in its main set, never in a
  `subset-of` fractional set.
- License must be
  `((GPL-2.0 WITH Linux-syscall-note) OR BSD-3-Clause)`.
- Specs must be self-contained: no dependency on other specs or C headers
  except via the `header` property for shared constants (e.g. `IFNAMSIZ`).
- Each major property should carry a `doc`.
- Prefer `notify:` (shares contents with a GET op) over `event:` (custom
  subset). Events are considered less idiomatic, unless the information
  carried is not something user will ever want to query directly (GET).
- For `netlink-raw` sub-messages: the **selector attribute must appear before
  the sub-message attribute** in the message; resolution uses the value
  *closest* to the selector. Missing selector at the expected level is an
  error.
- YAML structs are implicitly C-packed. Natural alignment requires explicit
  padding members - do not assume the compiler will add holes.
- Names in YAML spec should contain dashes, not underscores (code gen converts them).
