# HYGON PROCESSOR SUPPORT

This component implements x86 CPU vendor initialization, capability detection, NUMA node mapping, and TLB geometry probing for Hygon ("HygonGenuine") processors. It registers `hygon_cpu_dev` into the `.x86_cpu_dev.init` linker section so the core x86 CPU identification infrastructure invokes its early, BSP, per-CPU, and TLB detection callbacks during boot and CPU hotplug bringup.

## Architecture & Data Flow

- `init_hygon()` must invoke `early_init_hygon(c)` before canonicalizing remaining per-CPU features because `identify_cpu()` clears `c->x86_capability` with `memset()` and calls `this_cpu->c_init(c)` without calling `this_cpu->c_early_init(c)` on both the BSP and secondary CPUs. Broken by: removing `early_init_hygon(c)` from `init_hygon()`, or guarding capabilities in `early_init_hygon()` so they are skipped when called from `init_hygon()`. [F16]
- In `bsp_init_hygon()`, when `X86_FEATURE_AMD_SSBD` and `X86_FEATURE_VIRT_SSBD` are absent, `MSR_AMD64_LS_CFG` must be sampled via `rdmsrq_safe()` into `x86_amd_ls_cfg_base` before forcing `X86_FEATURE_LS_CFG_SSBD` and `X86_FEATURE_SSBD` and setting `x86_amd_ls_cfg_ssbd_mask`; SSBD must not be enabled if that read faults. [F14]
- In `init_hygon()`, when `cpu_has(c, X86_FEATURE_XMM2)` is true, setting `MSR_AMD64_DE_CFG_LFENCE_SERIALIZE_BIT` in `MSR_AMD64_DE_CFG` via `msr_set_bit()` must be paired with `set_cpu_cap(c, X86_FEATURE_LFENCE_RDTSC)` so `pm_save_spec_msr()` saves and restores `MSR_AMD64_DE_CFG` across suspend/resume. [F19]
- In `init_hygon()`, when `cpu_has(c, X86_FEATURE_SVM)` is true, `MSR_VM_CR` must be read and `X86_FEATURE_SVM` cleared via `clear_cpu_cap()` if `SVM_VM_CR_SVM_DIS_MASK` is set, keeping `c->x86_capability` synchronized with BIOS-locked hardware SVM state. [F18]
- `init_hygon()` must explicitly call `clear_cpu_cap(c, X86_FEATURE_APIC_MSRS_FENCE)` because `identify_cpu()` sets `X86_FEATURE_APIC_MSRS_FENCE` by default on every CPU before invoking `this_cpu->c_init(c)`. [F20]
- `init_hygon()` must gate `set_cpu_bug(c, X86_BUG_SYSRET_SS_ATTRS)` on `!cpu_feature_enabled(X86_FEATURE_XENPV)` because Xen resets SS attributes on `SYSRET` and the kernel's `X86_BUG_SYSRET_SS_ATTRS` context-switch workaround loads ring-0 `__KERNEL_DS` into `%ss`. [F66]
- In `srat_detect_node()`, when `!node_online(node)` holds, looking up `ht_nodeid` (`c->topo.initial_apicid`) in `__apicid_to_node[ht_nodeid]` must be followed by a sequential `if (!node_online(node))` check (not `else`) that falls back to `nearby_node(apicid)` using `c->topo.apicid`. [F33]
- `cpu_detect_tlb_hygon()` must check `c->extended_cpuid_level < 0x80000006` before reading CPUID leaf `0x80000006`, masking L2 TLB counts (`tlb_lld_4k`, `tlb_lli_4k`, `tlb_lld_2m`, `tlb_lli_2m`) from leaf `0x80000006` with 12-bit `mask` (`0xfff`). Whenever the L2 2M DTLB (`(eax >> 16) & 0xfff`) or L2 2M ITLB (`eax & 0xfff`) field is zero, it must fall back to L1 2M TLB counts from leaf `0x80000005` masked with 8-bit `0xff` so upper associativity bits in `eax` are excluded. [F39, F49]

## Synchronization

- In `bsp_init_hygon()`, `x86_amd_ls_cfg_base` and `x86_amd_ls_cfg_ssbd_mask` must only be initialized during early BSP initialization and never modified in `init_hygon()`, because both globals are `__ro_after_init` and are read without locks by `amd_set_core_ssb_state()` to avoid read-modify-write races on `MSR_AMD64_LS_CFG`. [F2]
- `cpu_detect_tlb_hygon()` mutates unguarded global TLB size variables (`tlb_lld_4k`, `tlb_lli_4k`, `tlb_lld_2m`, `tlb_lld_4m`, `tlb_lli_2m`, `tlb_lli_4m`) and must only be registered as `hygon_cpu_dev.c_detect_tlb` (invoked once on the BSP in `identify_boot_cpu()`) and never called from `init_hygon()` or per-CPU bringup paths. [F4]
- `init_hygon()`, `early_init_hygon()`, `srat_detect_node()`, and `nearby_node()` execute on the target CPU during secondary CPU bringup (`identify_secondary_cpu()`) with local interrupts and preemption disabled before the CPU is marked online, so they must not sleep or perform blocking allocations. [F52]

## Object Lifetimes

- `hygon_cpu_dev` and the call chain reachable from `hygon_cpu_dev.c_init` (`init_hygon`, `early_init_hygon`, `srat_detect_node`, `nearby_node`) must not be placed in `__init` or `__initconst` sections because `init_cpu_devs()` copies `&hygon_cpu_dev` from `.x86_cpu_dev.init` into persistent `cpu_devs[]` for later CPU hotplug bringup. [F1]

## API Contracts

- CPU capabilities enabled in `bsp_init_hygon()` (`X86_FEATURE_LS_CFG_SSBD` and `X86_FEATURE_SSBD`) must be set via `setup_force_cpu_cap()` rather than `set_cpu_cap(c, ...)`, because `identify_cpu()` later zeroes `boot_cpu_data.x86_capability` without re-running `c_bsp_init` and only preserves bits recorded in `cpu_caps_set[]`. [F62]
- In `init_hygon()`, `msr_set_bit()` must be called with a bit index (`MSR_AMD64_DE_CFG_LFENCE_SERIALIZE_BIT`, `0..63`) rather than a shifted bitmask (`MSR_AMD64_DE_CFG_LFENCE_SERIALIZE`), and its negative return value when `MSR_AMD64_DE_CFG` is absent is benign because `LFENCE` is already serializing on those families. Broken by: passing `BIT()`/`BIT_ULL()` to `msr_set_bit()` or skipping `set_cpu_cap(c, X86_FEATURE_LFENCE_RDTSC)` when `msr_set_bit()` returns `< 0`. [F10]
- In `early_init_hygon()`, `bsp_init_hygon()`, and `init_hygon()`, `set_cpu_cap()`, `clear_cpu_cap()`, `set_cpu_bug()`, `setup_force_cpu_cap()`, and `cpu_has()` must receive bit indices (`X86_FEATURE_*`, `X86_BUG_*`, or `0*32+31`) into `c->x86_capability`, whereas `c->x86_power` is tested with shifted masks (`BIT(n)`). [F42]
- `bsp_init_hygon()` must call `resctrl_cpu_detect(c)` in lockstep with `bsp_init_amd()` and `bsp_init_intel()` so `boot_cpu_data` cache monitoring fields (`x86_cache_max_rmid`, `x86_cache_occ_scale`, `x86_cache_mbm_width_offset`) are populated during BSP initialization. [F17]
- `srat_detect_node()` resolves the target CPU index via `smp_processor_id()` when calling `numa_cpu_node(cpu)` and `numa_set_node(cpu, node)`, requiring `init_hygon()` and `srat_detect_node()` to be invoked on the CPU being identified with preemption disabled. [F53]
- In `srat_detect_node()`, `x86_cpuinit.fixup_cpu_id` must be checked for non-NULL (since platforms without a multi-fabric CPU ID fixup handler leave it NULL) and invoked only after `node` is resolved from `numa_cpu_node(cpu)` (falling back to `c->topo.llc_id` when `NUMA_NO_NODE`) and before checking `node_online(node)` and calling `numa_set_node(cpu, node)`. [F7, F51]

## Error Handling

- Unchecked MSR reads `rdmsrq(MSR_K7_HWCR, val)` in `bsp_init_hygon()` and `rdmsrq(MSR_VM_CR, vm_cr)` in `init_hygon()` must be gated on `cpu_has(c, X86_FEATURE_CONSTANT_TSC)` and `cpu_has(c, X86_FEATURE_SVM)` respectively before reading the MSR. [F11]
- Probing optional MSRs (`MSR_AMD64_LS_CFG`, `MSR_AMD64_PATCH_LEVEL`, `MSR_AMD64_DE_CFG`) and reporting BIOS/firmware misconfigurations in `bsp_init_hygon()`, `early_init_hygon()`, and `init_hygon()` must use fault-safe MSR helpers (`rdmsrq_safe`, `rdmsr_safe`, `msr_set_bit`) and ratelimited or firmware-tagged logs (`pr_warn(FW_BUG ...)` and `pr_notice_once`) rather than `rdmsrq()`/`wrmsrq()` or `WARN_ON()`. [F68]

## External Contracts

- `hygon_cpu_dev.c_ident[0]` must remain a non-NULL, NUL-terminated 12-character string (`"HygonGenuine"`) because `get_cpu_vendor()` compares the 12-byte CPUID leaf 0 vendor string in `c->x86_vendor_id` against `cpu_devs[i]->c_ident[0]` via `strcmp()` without checking `c_ident[0]` for NULL. [F43]
- `early_init_hygon()` must set `X86_FEATURE_VMMCALL` on `struct cpuinfo_x86 *c` so that KVM paravirt hypercall alternatives patch `vmcall` to the AMD/Hygon `vmmcall` instruction. [F55]

## Known Pitfalls

- In `cpu_detect_tlb_hygon()`, `tlb_lld_4m` and `tlb_lli_4m` must be updated in lockstep as `tlb_lld_2m >> 1` and `tlb_lli_2m >> 1`, and the L1 DTLB fallback (`!((eax >> 16) & mask)`) must query leaf `0x80000005` without overwriting local `eax` from leaf `0x80000006` before the L2 ITLB check `!(eax & mask)`. Broken by: calling `cpuid(0x80000005, &eax, ...)` in the DTLB fallback branch, or updating `tlb_lld_2m`/`tlb_lli_2m` without recalculating `tlb_lld_4m`/`tlb_lli_4m`. [F21]
- `srat_detect_node()` and `nearby_node()` must index `__apicid_to_node[]` by APIC ID (`c->topo.initial_apicid` or `c->topo.apicid` within `[0, MAX_LOCAL_APIC)`), while passing the logical CPU number `cpu` (`smp_processor_id()`) to `numa_cpu_node()` and `numa_set_node()`. [F25]
- In `nearby_node()` and `srat_detect_node()`, any node ID retrieved from `numa_cpu_node()` or `__apicid_to_node[]` must be checked against `NUMA_NO_NODE` (`-1`) before calling `node_online(node)`, because `node_online()` calls `test_bit()` on `node_states[N_ONLINE].bits` without bounds checking. [F41]
- In `nearby_node()`, the loop counter `i` must be a signed integer (`int`) and the upper scan must remain bounded by `< MAX_LOCAL_APIC` so that `for (i = apicid - 1; i >= 0; i--)` terminates when `apicid == 0` or `i` reaches `-1` and `__apicid_to_node[i]` is never indexed out of bounds. [F26]
