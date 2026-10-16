# PCI Subsystem Details

Everything below describes properties a well-formed PCI patch should
satisfy.  When reviewing, CHECK the patch under review against these
properties and FLAG deviations.  Do not edit, rewrite, or perform any of
these actions yourself.  An imperative ("Add ...", "Strip ...", "Wrap ...")
describes what the author should have done; flag it if they did not.
Flag a violation based on lines the patch ADDS; you may read
surrounding context to confirm whether a matching release or guard is
present, but never flag pre-existing code for its own sake.

Every finding has one of three dispositions:

- **REPORT as bugs**: code-correctness invariants; report as a bug.
- **Low-severity suggestion**: prose, spelling, formatting, style.
- **Silent**: unsure, or evidence insufficient to confirm.

When unsure whether a rule applies, or when the evidence needed to
confirm a violation is not visible in the diff, stay silent.  Check
the code-correctness
invariants (the sections marked **REPORT as bugs**) before prose,
spelling, and formatting items, so that under an output budget the
bugs are reported first.  This file is a subsystem module loaded after
`review-core.md`, which owns output format and Fixes-tag policy.
The prose, spelling, and style sections below apply only when
subjective review is enabled; render their findings per
`inline-template.md`.  Every REPORT-as-bugs finding contributes at
least `low` severity to the `review-core.md` Task 5 aggregate.
These dispositions (REPORT as bugs, Low-severity suggestion, Silent)
are internal triage labels; never emit them in the review output.
Report each distinct defect once, even if it matches multiple
sections.

## Commit Message Title Format

Using the wrong subject prefix or capitalization style makes the patch
look out of place in `git log --oneline` output and forces the maintainer
to silently rewrite it before applying, delaying the patch.

Check the subject prefix against the conventions below and the path of the
file being changed.  You have only the patch; do not claim to have run
`git log` or compared against history.  The PCI subsystem uses
hierarchical prefixes separated by colons:

- Core PCI code: `PCI: <Description>`
- PCI features (MSI, ASPM, PM, TPH, etc): `PCI/<Feature>: <Description>`
- Native host bridge drivers: `PCI: <driver>: <Description>`
- DT bindings: `dt-bindings: PCI: <binding-or-vendor>: <Description>`
- Endpoint: `PCI: endpoint: <Description>`

The description starts with a capitalized verb and forms a complete
sentence without a trailing period:

```
// CORRECT
PCI: altera: Fix platform_get_irq() error handling
PCI: rockchip: Remove IRQ affinity so we can allocate more IRQs
PCI: mediatek: Add MSI support for MT2712 and MT7622
dt-bindings: PCI: andes: Add Andes QiLai PCIe controller

// WRONG - lowercase after colon
PCI: altera: fix platform_get_irq() error handling

// WRONG - trailing period
PCI: rockchip: Remove IRQ affinity.
```

**Low-severity suggestion.**

## Commit Message and Version History

Posting a new version without describing what changed forces every
reviewer to re-diff the entire series to figure out what is new, wasting
their time and delaying review.

- If the subject line carries a version tag (`[PATCH v2]`, `[v3]`, etc.),
  a changelog should appear below the `---` line describing what changed.
  Flag its absence only when a version tag is present; do not flag a
  missing changelog on a first posting.
- A changelog should link to the previous version of the patch or series.
  Links to prior art or related discussions are helpful when relevant.
- The commit message body should read on its own, not as a continuation of
  the subject line.
- Initialisms ("PCI", "IRQ", "ID", "MSI", "ASPM", "AER", "DMA", "BAR")
  are capitalized in all English text: title, commit message, comments.
  See also the Spelling and Terminology section.
- Function names carry `()` and array names carry `[]` in the commit
  message; flag only when you can confirm the identifier is a function
  or array from the diff or tree (see the Referring to Functions
  section).
- The commit message body contains no tabs; `git log` indents it and
  tab-aligned content drifts.  Trailers such as `Cc: stable` are exempt.
- The commit message body is plain text; no markdown.  Flag markdown
  formatting (fenced code blocks, block-quote prefixes).  Quoted
  material is to be indented by two spaces, not set off with markdown;
  this indentation does not trigger the no-tabs rule above.

**Low-severity suggestion.**

## Pasting dmesg Output and Error Messages

Timestamps and function addresses in pasted dmesg output are noise that
clutters the commit message and makes the actual error harder to find with
search engines.

- Pasted dmesg output in the commit message has timestamps and bare
  function addresses stripped.  Flag dmesg or error output in the commit
  message text that still carries `[   12.345678]`-style timestamps or raw
  hex function addresses.  Do not apply this to C source code in the diff
  (hex literals in code are not dmesg timestamps).
- Irrelevant dmesg lines are pruned to the lines that illustrate the issue.
- Pasted error messages, stack traces, and compiler output are not wrapped
  to fit a column limit; they stay on one line so they remain searchable
  with `grep`.

```
// CORRECT - timestamps stripped, addresses stripped, one line
  pci 0000:00:1c.0: PME# disabled
  BUG: unable to handle page fault for address: ffff8881a0000000
  Call Trace:
    pci_write_config_word
    pci_set_power_state

// WRONG - timestamps and symbol offsets retained, error wrapped
  [   12.345678] pci 0000:00:1c.0: PME# disabled
  [   12.345679] BUG: unable to handle page fault for address:
                 ffff8881a0000000
  [   12.345680] Call Trace:
  [   12.345681]  pci_write_config_word+0x1a8/0x2c0
  [   12.345682]  pci_set_power_state+0x84/0x120
```

**Low-severity suggestion.**

## Commit Message Scope

Lengthy architectural discussion, design rationale, and background in an
individual patch commit message make it hard to review and clutter
`git log` permanently.

Judge only the commit message in front of you.  An individual patch commit
message should be concise: what the patch changes, why, and any non-obvious
consequence.  Flag a commit message body whose prose (excluding trailers,
pasted dmesg, and call-chain blocks) exceeds 24 lines.  Do not assert
that content "belongs in the cover letter"; you cannot see whether a
cover letter exists.

**Low-severity suggestion.**

## Call Chain Presentation

A commit message that describes a race or crash path without the actual
call chain forces the reviewer to reconstruct it from source.

When a commit message includes a call chain, expect indented function
names with hex addresses stripped and only the relevant frames shown.
Flag retained hex addresses on patch-added call-chain lines.  In a call
chain where indentation shows the nesting, function names need not
carry `()`.

```
  pci_create_sysfs_dev_files
    sysfs_create_bin_file                 (resourceN)
      -> -EEXIST if file already created by another thread
```

**Low-severity suggestion.**

## Line Length

These are low-severity style items the maintainer enforces, not bugs.

The PCI subsystem wraps code and comments to 80 columns.  Lines over 80
columns are routinely flagged or reformatted by the maintainer.

- Flag patch-added C-code and comment lines that exceed 80 columns.
  Measure the actual line width; do not estimate it.  Do not apply this
  to commit-message prose, pasted dmesg output, or call-chain diagrams
  (see the dmesg and Call Chain sections for those).
- Do not flag a line whose overflow is unavoidable: a single unbreakable
  token (a long identifier, vendor or spec name, or string literal) that
  already begins at or near the left margin and cannot be wrapped.
- Do not flag a printk or `dev_*` format string that runs past 80 columns;
  it is kept whole so the message stays searchable with `grep`.  Do flag a
  format string split across adjacent string literals (two quoted strings
  with nothing but whitespace between them in a printk/`dev_*` call).

```c
// CORRECT - searchable
dev_err(dev, "Failed to enable PCI device with error %d\n", ret);

// WRONG - split across adjacent string literals
dev_err(dev, "Failed to enable PCI device"
        " with error %d\n", ret);
```

## PCI Bus Address vs Physical Address

Confusing `pci_bus_addr_t` with `phys_addr_t` causes incorrect address
translations when an IOMMU or host bridge window applies an offset between
CPU physical addresses and PCI bus addresses.

- `pci_bus_addr_t`: an address valid on the PCI side of a host bridge, i.e.
  what the device sees.
- `phys_addr_t`: a CPU physical address, valid on the CPU side.  These
  differ under an IOMMU or host bridge address translation.
- A value from `pci_bus_address()` must be stored in `pci_bus_addr_t`, not
  `phys_addr_t` or `resource_size_t`.

**REPORT as bugs**: a patch that stores `pci_bus_address()` into a
`phys_addr_t` or `resource_size_t` variable, or passes it to a function
expecting a CPU physical address.

## Root Port Property Placement in Devicetree

Placing per-connector signals (`reset-gpios`/PERST#, `wake-gpios`/WAKE#)
directly in the Root Complex node prevents supporting future hardware
that has multiple Root Ports in a single Root Complex, requiring a
breaking DT schema change.

Per-connector signals belong in their own Root Port child node, not in the
parent RC node:

- PERST# GPIO: a per-connector signal driven to the endpoint.
- Wake GPIO: a per-connector signal sensed at the Root Port level.

`num-lanes` is legitimately a controller-node property on most single-port
bindings (e.g. `snps,dw-pcie`, `fsl,imx6q-pcie`) and must not be flagged
at the controller node.

```dts
/* CORRECT - per-connector signals in Root Port node */
pcie@f0000000 {
    compatible = "vendor,pcie";
    pcie@0,0 {
        reg = <0x000000 0 0 0 0>;
        reset-gpios = <&gpio 5 GPIO_ACTIVE_LOW>;
    };
};

/* WRONG - per-connector signal in RC node */
pcie@f0000000 {
    compatible = "vendor,pcie";
    reset-gpios = <&gpio 5 GPIO_ACTIVE_LOW>;
};
```

**REPORT as bugs**: a per-connector signal property (PERST# GPIO, wake
GPIO) placed in the Root Complex node instead of a Root Port child
node.  Do not flag controllers whose DT schema already places these
at the controller node.

## PCIe Timing Compliance

Missing or wrong PCIe reset and link-training delays cause link training
and enumeration failures on hardware that enforces strict timing.

You cannot verify hardware timing from a patch, so do not assert a spec
timing violation.  What is checkable: when a patch adds or changes a PERST#
or link-training delay, a delay should be present and expressed with a
named constant (e.g. `PCIE_T_PVPERL_MS`), not a bare magic number.  Do
not flag delays whose value comes from a DT property or a parameterized
source (the value is not a magic number in that case).

Low-severity suggestion; not a bug.

## Config-Space Accessor Width Mismatch

The PCI config-space accessors take a pointer or value whose width must
match the accessor: `pci_read_config_byte()` writes a `u8`,
`pci_read_config_word()` a `u16`, `pci_read_config_dword()` a `u32`.
The write accessors (`pci_write_config_byte()`, `pci_write_config_word()`,
`pci_write_config_dword()`) have the mirror-image hazard: passing a wider
value truncates or writes the wrong bytes on big-endian.

The same width rule applies to `pcie_capability_read_word()` (`u16 *`),
`pcie_capability_read_dword()` (`u32 *`), and the write variants.

```c
// CORRECT
u8 cap;
pci_read_config_byte(dev, PCI_CAPABILITY_LIST, &cap);

// WRONG - mismatched width hidden by a cast; wrong byte on big-endian
u32 val;
pci_read_config_byte(dev, PCI_CAPABILITY_LIST, (u8 *)&val);
```

**REPORT as bugs**: any config-space or capability accessor passed a
pointer or value cast to a mismatched width.  On big-endian
architectures, this silently accesses the wrong byte.

## Capability Lookup Return Value

`pci_find_capability()` and `pci_find_ext_capability()` return an
unsigned offset (0 on not-found).  A `< 0` check on their return value
is dead code.

**REPORT as bugs**: `pci_find_capability()` or
`pci_find_ext_capability()` return value compared with `< 0`.

## PCIBIOS Return-Code Semantics

`pci_read_config_byte()`, `pci_read_config_word()`,
`pci_read_config_dword()`, `pci_write_config_byte()`,
`pci_write_config_word()`, `pci_write_config_dword()`,
`pcie_capability_read_word()`, `pcie_capability_read_dword()`,
`pcie_capability_write_word()`, `pcie_capability_write_dword()`, and
the read-modify-write helpers `pcie_capability_clear_and_set_word()`,
`pcie_capability_clear_and_set_dword()`, `pcie_capability_set_word()`,
and `pcie_capability_clear_word()` return PCIBIOS status codes
(`PCIBIOS_SUCCESSFUL` (0), `PCIBIOS_FUNC_NOT_SUPPORTED`,
`PCIBIOS_BAD_VENDOR_ID`, `PCIBIOS_DEVICE_NOT_FOUND`,
`PCIBIOS_BAD_REGISTER_NUMBER`, `PCIBIOS_SET_FAILED`,
`PCIBIOS_BUFFER_TOO_SMALL`; all positive), not negative errno.  Two
common bugs: (a) returning the raw result as errno without
`pcibios_err_to_errno()`; (b) checking `if (ret < 0)`, which is dead
code.

`pci_clear_and_set_config_dword()` returns void; it is not subject to
this rule.

**REPORT as bugs**: any of the accessors or RMW helpers listed above
whose return value is checked with `< 0` or returned directly as a
negative errno.  Do not flag a `< 0` check on a value that has been
converted through `pcibios_err_to_errno()` first; that conversion
produces a proper negative errno and `< 0` is correct.

## Unchecked PCI Init Return Values

`pci_enable_device()`, `pci_enable_device_mem()`,
`pci_request_regions()`, and `pci_request_selected_regions()` return
an int error code that must be checked.  The return value is unchecked
when it is discarded (bare call statement) or assigned to a variable
that is never tested before the next use of the resource.

**REPORT as bugs**: any of the above calls whose return value is not
checked (bare statement or assigned but never tested).

## Pointer Error-Return Conventions (beyond Endpoint)

PCI host bridge and controller drivers use mapping and resource
functions that differ in their failure return:

- Return NULL on failure: `ioremap()`, `pci_ioremap_bar()`,
  `of_iomap()`, `devm_ioremap()`.  Check with `if (!ptr)`.
- Return ERR_PTR on failure: `devm_ioremap_resource()`,
  `devm_clk_get()`, `devm_reset_control_get()` and variants.
  Check with `if (IS_ERR(ptr))`.

Using `if (!ptr)` on an ERR_PTR function lets a poison pointer
through and silently swallows `-EPROBE_DEFER`.

**REPORT as bugs**: a return from an ERR_PTR function listed above
checked with `!ptr` or `== NULL`, or a return from a NULL function
checked with `IS_ERR()`.

## Raw Config Read on PCIe Capability Registers

Reading PCIe Capability registers with `pci_read_config_word()` or
`pci_read_config_dword()` using a hardcoded offset fails on PCIe v1
devices where the capability is at a different position.
`pcie_capability_read_word()` and `pcie_capability_read_dword()`
handle both v1 and v2 layout transparently.

**REPORT as bugs**: `pci_read_config_word()` or
`pci_read_config_dword()` used with an offset whose macro name begins
with `PCI_EXP_` (e.g. `PCI_EXP_LNKCTL`, `PCI_EXP_DEVCTL`,
`PCI_EXP_LNKSTA`); use
`pcie_capability_read_word()` / `pcie_capability_read_dword()` instead.
The same applies to writes.  Do not flag other config-space registers.
When the offset is a bare literal or an unrecognized macro, stay
silent.  A line may also trigger Config-Space Accessor Width Mismatch;
these are distinct defects (wrong accessor vs wrong width).

## pci_irq_vector() Return Value

`pci_irq_vector()` returns a negative errno on failure.  Storing it
in an unsigned variable (e.g. `unsigned int irq`) silently converts
the error to a large positive value, which is then passed to
`request_irq()` and causes a bogus IRQ allocation.

**REPORT as bugs**: `pci_irq_vector()` return value stored in an
unsigned variable.

## Driver-Local PCI Register Defines

Redefining PCI register offsets or field masks that already exist in
`include/uapi/linux/pci_regs.h` creates duplicates that drift from the
canonical definitions and clutter the driver.

**REPORT as bugs**: a `#define` that
duplicates a constant already in `pci_regs.h`.

## PERST# GPIO Sleep Context

PERST# GPIOs may sit behind an I2C or SPI expander, which requires a
sleeping bus transaction.  `gpiod_set_value()` is atomic-only and
will warn or fail on a sleeping GPIO.

Flag `gpiod_set_value()` used to assert or deassert PERST# on a line
as a low-severity suggestion:
`gpiod_set_value_cansleep()` is needed when the GPIO sits behind a
sleeping bus, but `gpiod_set_value()` is correct for direct-MMIO
GPIOs.  The diff cannot show which case applies.  Report only when
the hunk shows the descriptor is the PERST#/reset GPIO (variable
name containing "reset" or "perst", a `devm_gpiod_get(..., "reset",
...)` call, or a comment).

## DMA Mask Pairing

`dma_set_mask()` without a matching `dma_set_coherent_mask()` leaves
the coherent mask at its default (often 32-bit), causing coherent
allocations to fail or fall back unnecessarily on hardware that
supports a wider mask.

**REPORT as bugs**: `dma_set_mask()` with
no `dma_set_coherent_mask()` within the visible hunk.  Do not flag
`dma_set_mask_and_coherent()`, which sets both.

## Code Style

These are low-severity readability preferences the PCI maintainer
enforces, not bugs.

- Flag a local variable initialized to a bare literal or a single named
  constant and read exactly once; the constant belongs at the use site,
  e.g. `int dma_bits = 64;` used once should just be `64`.

## PCI Endpoint Error Return Conventions

Passing an error pointer to a function expecting a valid `struct pci_epc *`
or `struct pci_epf *` crashes when the pointer is dereferenced.
`pci_epf_destroy()` dereferences its argument unconditionally (to call
`device_unregister()`), so an ERR_PTR crashes it.  `pci_epc_put()` is safe
because it checks `IS_ERR_OR_NULL()` first.

These functions return `ERR_PTR()` on failure, not NULL:

- `pci_epc_get()` (`drivers/pci/endpoint/pci-epc-core.c`)
- `pci_epf_create()` (`drivers/pci/endpoint/pci-epf-core.c`)

**REPORT as bugs**: a return from `pci_epc_get()` or `pci_epf_create()`
checked with `!ptr` instead of `IS_ERR()`, or an ERR_PTR value passed to
`pci_epf_destroy()`.

## Legacy PCI MSI APIs

New code using the legacy MSI APIs lacks MSI-X support and the error
handling of the modern IRQ vector interface.  `drivers/pci/msi/api.c`
labels `pci_enable_msi()`, `pci_disable_msi()`, `pci_enable_msix_range()`,
and `pci_disable_msix()` as "Legacy device driver API".
`pci_enable_msix_exact()` is a static inline in `include/linux/pci.h`
that wraps the legacy `pci_enable_msix_range()` and is equally legacy.
New code should use `pci_alloc_irq_vectors()` /
`pci_free_irq_vectors()`.

```c
// WRONG - legacy API, no MSI-X
ret = pci_enable_msi(pdev);
if (ret)
    return ret;

// CORRECT - supports MSI, MSI-X, and legacy INTx
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;
```

**REPORT as bugs**: `pci_enable_msi()`, `pci_enable_msix_range()`, or
`pci_enable_msix_exact()`, when no matching
`-` line removes the same call (i.e. it is genuinely new, not moved).
Do not flag `pci_disable_msi()` or `pci_disable_msix()` added as
cleanup in a `remove()` path of a driver already on the legacy API.

## PCI IRQ Vector Cleanup in Error Paths

Failing to call `pci_free_irq_vectors()` after a successful
`pci_alloc_irq_vectors()` leaks IRQ resources, preventing future
allocations and potentially exhausting system IRQ capacity.

```c
// WRONG - IRQ vectors leaked on init failure
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;
ret = some_init(pdev);
if (ret)
    return ret;  // BUG: vectors not freed

// CORRECT
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;
ret = some_init(pdev);
if (ret)
    goto free_irq;
return 0;

free_irq:
    pci_free_irq_vectors(pdev);
    return ret;
```

**REPORT as bugs**: an error path or `remove()` path that the patch
ADDS or modifies, after a successful `pci_alloc_irq_vectors()`, that
returns without calling `pci_free_irq_vectors()`.  Do not flag
pre-existing leaky paths that the patch did not touch.  Do not flag
paths that reach a shared cleanup label (e.g. `goto free_irq`) which
calls `pci_free_irq_vectors()`, or code using `devm_`/`pcim_`-managed
IRQ allocation.
See also the Resource Lifetime section for the general acquire/release
pairing rule; report once, not twice.

`pci_alloc_irq_vectors()` returns a positive vector count on success.
Checking the result with `if (ret)`, `if (!ret)`, or `if (ret != 0)`
treats success as failure.  The correct test is `if (ret < 0)`.

**REPORT as bugs**: `pci_alloc_irq_vectors()` return value checked with
`if (ret)`, `if (!ret)`, or `if (ret != 0)`,
when the call and the check use the same variable.

## Device Naming Conventions

Renaming a parent or bus device from a driver confuses sysfs, breaks
userspace tools expecting standard naming, and interferes with PCI device
management.  The PCI subsystem owns device naming.

```c
// WRONG - driver renames its parent PCI device
dev_set_name(&pdev->dev, "my-device");

// CORRECT - naming a child device the driver itself created
struct device *child = kzalloc(sizeof(*child), GFP_KERNEL);
dev_set_name(child, "child-%d", id);
```

**REPORT as bugs**: `dev_set_name()` called on `&pdev->dev`.  For any
other device, flag only when the hunk shows it is not driver-allocated
(passed in or a parent device).

## Devicetree Node Refcounting

A `device_node` obtained from `of_get_child_by_name()`,
`of_parse_phandle()`, `of_find_node_by_*()`, or a non-scoped
`for_each_child_of_node()` / `for_each_available_child_of_node()` loop
holds a reference that must be released with `of_node_put()` on every exit
path, including early `break`, `return`, and `goto`.

The `_scoped()` iterator variants and `__free(device_node)` release
automatically and need no manual put.

**REPORT as bugs**: either of:

- A patch that ADDS one of the node-getter calls listed above with no
  `of_node_put()` on a reachable exit path visible in the diff.
- A patch that ADDS a new early return, break, or goto past a
  pre-existing or added node-getter or non-scoped iterator, without
  `of_node_put()` on that path.

Do not flag `_scoped()` iterators or `__free()`-managed nodes.

## PCI Device Refcounting

`pci_get_domain_bus_and_slot()`, `pci_get_slot()`, `pci_get_device()`,
`pci_get_class()`, and `pci_dev_get()` return a counted reference that must
be released with `pci_dev_put()` on every exit path.
`__free(pci_dev_put)` releases automatically.

Note: `pci_get_device()` and `pci_get_class()` used as iterators (the
`from` argument carries the previous reference) drop the reference on loop
continuation, so only early exits from such a loop need a manual
`pci_dev_put()`.

**REPORT as bugs**: either of:

- A patch that ADDS one of these getters with no `pci_dev_put()` on
  a reachable exit path visible in the diff.
- A patch that ADDS a new early exit (break, return, goto) past a
  pre-existing or added getter, without `pci_dev_put()` on that path.

Do not flag when the reference is managed by `__free(pci_dev_put)`.
For `pci_get_device()`/`pci_get_class()` used as iterators, only an
early break/return/goto out of the loop leaks; normal loop completion
re-passes the reference and is not a leak.

## Resource Lifetime in probe() / remove()

A resource acquired with a non-devm API in `probe()` (`pci_enable_device()`,
`ioremap()`, `clk_get()`, `reset_control_get()`, `pci_alloc_irq_vectors()`,
`kzalloc()`, `pci_epc_mem_alloc_addr()`, `pci_request_regions()`, etc.)
must be released in `remove()` and on every `probe()` error path.  Note
that `pcim_enable_device()` is the managed equivalent of
`pci_enable_device()`.  A resource acquired with a `devm_` or `pcim_`
helper must NOT also be freed manually (double free).

Do not flag a `kzalloc()` whose result is passed to a framework that takes
ownership (e.g. `device_register()` or a `.release` callback); freeing it
manually after handing ownership is a double free.

**REPORT as bugs**:

- A patch that ADDS a non-devm acquire in `probe()` with no matching
  release on the error unwind path visible in the diff.  If the matching
  `remove()` or teardown function is not in the diff, do not report a
  leak; report only when the release site is visible and omits the free
  on a reachable path.
- A patch that ADDS a manual free (`kfree()`, `iounmap()`, `clk_put()`,
  `pci_disable_device()`, `pci_release_regions()`) of a resource the
  same patch acquired via a `devm_` or `pcim_` helper.

`pci_alloc_irq_vectors()`/`pci_free_irq_vectors()` pairing is owned by
the PCI IRQ Vector Cleanup section; report a missing IRQ-vector free
there, not here.

## Commit Message Prose Style

These are flag-and-suggest items for commit message prose.  On
text the patch ADDS, note the issue and suggest the fix.  Never edit the
patch yourself, and keep these as low-severity suggestions, not bugs.

- Imperative mood, not descriptive: "Add support", not "This adds support"
  or "This patch adds".
- No conversational openers: "So,", "Hence,", "As per", "Let's", "Let us".
  State the fact directly.
- In the body's opening sentence, prefer naming the action over describing
  an effect ("This allows the driver to ...").  Mid-body sentences may
  legitimately describe effects ("This reduces latency by 30%", "This
  allows hotplug to proceed without a rescan").
- "it's" is only "it is"; the possessive is "its".
- Precise verbs: prefer "reduce" over "improve", "translate" over "modify",
  "PCI error source device" over "erring PCI device".
- Parallel verb forms in a series title match the body ("Factor ... and
  Rename ..." gives "Factor", "Rename").
- Commas and periods sit inside quotation marks; nested quotes use single
  inside double: `"the 'max-link-speed' property"`.
- Subject lines use single quotes, because the common way to cite a commit
  wraps the subject in double quotes.

## Prose in Code Comments

Apply these only to comments the patch ADDS, never to context or
pre-existing comments, and keep them as low-severity suggestions.

- A comment sentence starts with an uppercase letter.
- A multi-line block comment ends its sentence with a period.  A short
  single-line comment need not, since a trailing period can push it over
  the line.
- Function documentation uses kernel-doc (`/** ... */` with `@param:`).

```c
// CORRECT
/* Enable MSI interrupts for this device */

/*
 * Wait for the link to come up before accessing configuration space.
 * The PCIe spec requires a minimum delay after PERST# deassertion
 * (PCIe r7.0, sec 6.6.1).
 */

// WRONG - lowercase start
/* enable MSI interrupts for this device */
```

## Spelling and Terminology

Apply these to prose the patch ADDS, never to code identifiers, file paths,
DT node names, or quoted strings, and keep them as low-severity
suggestions.

- Flag obvious misspellings of English words in the prose the patch adds.
- Capitalize initialisms and roles in prose: "PCI", "DMA", the PCIe
  "Endpoint" role.
- Signal names carry the spec hash and capitalization: `PERST#`, `WAKE#`,
  `CLKREQ#`.
- Compound terms are one word, no hyphen or space: "Downstream", "Upstream",
  "Retrain", "Rescan".
- PCIe spec terms use the spec's capitalization ("Completer", "Requester
  ID", "Message"), which doubles as a hint that they are defined in the
  spec.

## Referring to Functions, Structs, and Callbacks in Prose

Wrong notation makes commit-message and comment prose ambiguous about
whether the author means a function, a variable, a type, or a field.  Apply
only to prose the patch ADDS.

- Function names carry `()`: `pci_enable_device()`.  Flag a bare function
  name only when you can confirm from the diff or the tree that it is a
  function.
- Struct members use a dot, or `->` from a pointer: `struct pci_dev.irq`,
  `pdev->irq`.  Not `::` or `#`.
- Ops callbacks carry a leading dot and `()`: `.probe()`,
  `.suspend_noirq()`, so they read as callable members, not data fields.

**Low-severity suggestion.**
