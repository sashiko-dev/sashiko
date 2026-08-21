# Linux Review Prompt Index

This index maps kernel subsystems, file paths, and symbol patterns to domain-specific
review prompts categorized under `api/`, `subsystems/`, and `generic/`.

## Directory Structure
- `api/`: Invariants, rules, and safety guidelines for widely used APIs intended for callers across the tree.
- `subsystems/`: Invariants, internals, and driver specifics for kernel subsystems.
- `generic/`: High-level review guidance, severity calibration, formatting, and technical patterns.

## Subsystem and API Index

| Category | Subsystem / API | Triggers | Prompt File |
|----------|-----------------|----------|-------------|
| Generic | Technical Patterns | Core execution flow, resource lifecycles, error handling | generic/technical-patterns.md |
| Generic | Callstack Analysis | Deep callstack analysis, indirect call invariants | generic/callstack.md |
| Generic | False Positive Guide | Verification checks, defensive programming suppression | generic/false-positive-guide.md |
| Generic | Severity Calibration | Severity scoring rules and escalation thresholds | generic/severity.md |
| Generic | Inline Template | LKML email reporting style and inline comment format | generic/inline-template.md |
| Generic | Fixes Tag | Fixes tag syntax and commit SHA verification | generic/fixes-tag.md |
| API | Locking | `spin_lock*`, `mutex_*`, `rwsem*`, `seqlock*`, `guard(mutex)`, `guard(spinlock)` | api/locking.md |
| API | RCU Caller Rules | `rcu_read_lock`, `rcu_read_unlock`, `rcu_dereference*`, `rcu_assign_pointer`, `synchronize_rcu`, `call_rcu`, `kfree_rcu` | api/rcu.md |
| API | Cleanup & Scope | `__free`, `guard(`, `scoped_guard`, `DEFINE_FREE`, `DEFINE_GUARD`, `no_free_ptr`, `return_ptr` | api/cleanup.md |
| API | Memory Allocation | `alloc_pages`, `__GFP_*`, `kmalloc`, `kzalloc`, `kmem_cache_*`, `vmalloc`, `kvmalloc`, `mempool` | api/mm-alloc.md |
| API | Timers | `timer_list`, `timer_setup`, `mod_timer`, `del_timer_sync`, `hrtimer_*` | api/timers.md |
| API | Workqueue | `work_struct`, `delayed_work`, `schedule_work`, `queue_delayed_work`, `destroy_workqueue`, `cancel_work_sync` | api/workqueue.md |
| API | Alignment Helpers | `ALIGN`, `ALIGN_DOWN`, `IS_ALIGNED`, `PAGE_ALIGN`, `PAGE_ALIGN_DOWN`, `pageblock_align` | api/alignment.md |
| API | I/O Accessors | `readl`, `writel`, `readw`, `writew`, `readb`, `writeb`, `ioremap`, `iounmap`, `__raw_readl`, `__raw_writel` | api/io-accessors.md |
| API | Sysfs | `fs/sysfs/`, `sysfs_create_group`, `sysfs_update_group`, `attribute_group`, `DEVICE_ATTR_*` | api/sysfs.md |
| API | Pointer Guards | `IS_ERR`, `PTR_ERR`, `ERR_PTR`, `IS_ERR_OR_NULL` | api/pointer-guards.md |
| API | Open Firmware (DT) | `drivers/of/`, `of_node`, `of_find_*`, `of_get_*`, `of_parse_*`, `of_node_put`, `of_node_get` | api/of.md |
| API | DT Bindings | `Documentation/devicetree/bindings/`, `*.yaml` in devicetree bindings | api/dt-bindings.md |
| API | PCI | `drivers/pci/`, `pci_*` | api/pci.md |
| Subsystem | Networking Core | `net/`, `skb_`, `sockets`, `xfrm`, `dst_`, `sock_put`, `release_sock`, `pskb_may_pull` | subsystems/networking-core.md |
| Subsystem | Networking Drivers | `drivers/net/`, `ethtool_ops`, `net_device_ops`, `napi_*` | subsystems/networking-drivers.md |
| Subsystem | Netlink | `genl_`, `nla_`, `NLA_`, `NLM_F_`, `nlmsg_`, `netlink_callback`, `Documentation/netlink/specs/` | subsystems/netlink.md |
| Subsystem | MM Page Tables | `pte_*`, `pmd_*`, `pud_*`, `set_pte`, `ptep_*`, `tlb_*`, `mm/memory.c`, `mm/pagewalk.c` | subsystems/mm-pagetable.md |
| Subsystem | MM Folios | `folio_*`, `page_folio`, `compound_head`, `filemap_*`, `xa_*`, `mm/filemap.c` | subsystems/mm-folio.md |
| Subsystem | MM Large Pages | `huge_memory`, `hugetlb`, `split_huge_*`, `folio_test_large`, `mm/huge_memory.c` | subsystems/mm-largepage.md |
| Subsystem | MM VMA | `vma_*`, `mmap_*`, `vm_area_struct`, `vm_flags`, `anon_vma`, `maple_tree`, `mm/vma.c` | subsystems/mm-vma.md |
| Subsystem | MM Reclaim | `vmscan`, `shrink_*`, `lru_*`, `swap_*`, `mem_cgroup_*`, `mm/vmscan.c`, `mm/memcontrol.c` | subsystems/mm-reclaim.md |
| Subsystem | DAX | `dax_operations`, `dax_direct_IO`, `fs/dax.c`, `drivers/dax/` | subsystems/dax.md |
| Subsystem | VFS | `inode`, `dentry`, `vfs_`, `fs/*.c` | subsystems/vfs.md |
| Subsystem | Btrfs | `fs/btrfs/`, `btrfs_*` | subsystems/btrfs.md |
| Subsystem | Block Layer | `block/`, `bio_*`, `request_queue`, `blk_mq_*`, `drivers/nvme/` | subsystems/block.md |
| Subsystem | Encryption (fscrypt) | `fs/crypto/`, `fscrypt_*` | subsystems/fscrypt.md |
| Subsystem | FUSE | `fs/fuse/`, `fuse_*`, `FUSE_IO_URING` | subsystems/fuse.md |
| Subsystem | NFSD | `fs/nfsd/`, `fs/lockd/`, `nfsd_*` | subsystems/nfsd.md |
| Subsystem | SMB/ksmbd | `fs/smb/server/`, `ksmbd_*`, `smb_direct_*` | subsystems/smb-ksmbd.md |
| Subsystem | CXL | `drivers/cxl/`, `cxl_*` | subsystems/cxl.md |
| Subsystem | ATA/libata | `drivers/ata/`, `ata_dev_*`, `ata_port_*` | subsystems/ata.md |
| Subsystem | USB Storage | `drivers/usb/storage/`, `unusual_devs.h`, `USB_SC_*` | subsystems/usb-storage.md |
| Subsystem | io_uring | `io_uring/`, `io_uring_*`, `io_ring_*`, `IORING_*` | subsystems/io_uring.md |
| Subsystem | KHO | `lib/test_kho.c`, `kho_*`, `register_kho_notifier` | subsystems/kho.md |
| Subsystem | ARM64 | `arch/arm64/`, `sysreg` | subsystems/arm64.md |
| Subsystem | ARM64 Hyp (EL2) | `arch/arm64/kvm/hyp/`, `__hyp_` | subsystems/hyp-arm64.md |
| Subsystem | MIPS | `arch/mips/`, `tlb_probe`, `tlb_read` | subsystems/mips.md |
| Subsystem | KVM Core | `virt/kvm/`, `include/linux/kvm*`, `kvm_*` | subsystems/kvm.md |
| Subsystem | KVM ARM64 | `arch/arm64/kvm/` | subsystems/kvm-arm64.md |
| Subsystem | Scheduler | `kernel/sched/`, `sched_*`, `schedule` | subsystems/scheduler.md |
| Subsystem | BPF | `kernel/bpf/`, `tools/lib/bpf/`, `bpf_*` | subsystems/bpf.md |
| Subsystem | BTF | `map_check_btf`, `check_and_init_map_value`, `BPF_KPTR` | subsystems/btf.md |
| Subsystem | Libbpf | `tools/lib/bpf/`, `LIBBPF_API` | subsystems/libbpf.md |
| Subsystem | Tracing | `trace_*`, `tracepoints`, `kernel/trace/` | subsystems/tracing.md |
| Subsystem | Perf | `tools/perf/`, `kernel/events/` | subsystems/perf.md |
| Subsystem | Objtool | `tools/objtool/`, `INSN_BUG`, `INSN_TRAP` | subsystems/objtool.md |
| Subsystem | Syscalls | `SYSCALL_DEFINE`, `copy_from_user`, `copy_to_user` | subsystems/syscall.md |
| Subsystem | Build System | `Kbuild`, `Makefile`, `scripts/` | subsystems/build.md |
| Subsystem | Kconfig | `Kconfig`, `config `, `depends on ` | subsystems/kconfig.md |
| Subsystem | Rust | Kernel Rust code | subsystems/rust.md |
| Subsystem | DRM/GPU | `drivers/gpu/drm/`, `drm_*` | subsystems/drm.md |
| Subsystem | I2C | `drivers/i2c/`, `i2c_*` | subsystems/i2c.md |
| Subsystem | HID | `drivers/hid/`, `hid_*` | subsystems/hid.md |
| Subsystem | Input | `drivers/input/`, `input_*` | subsystems/input.md |
| Subsystem | Hardware Monitoring | `drivers/hwmon/`, `hwmon_*` | subsystems/hwmon.md |
| Subsystem | LEDs | `drivers/leds/`, `led_classdev_*` | subsystems/leds.md |
| Subsystem | Media/V4L2 | `drivers/media/`, `include/media/`, `v4l2_subdev_*`, `MEDIA_BUS_FMT_*` | subsystems/media.md |
| Subsystem | MFD | `drivers/mfd/`, `mfd_*` | subsystems/mfd.md |
| Subsystem | Power Management | `drivers/base/power/`, `pm_runtime_*` | subsystems/pm.md |
| Subsystem | Power Domains | `drivers/pmdomain/`, `pm_genpd_*` | subsystems/pmdomain.md |
| Subsystem | TTY / Serial | `drivers/tty/`, `uart_*` | subsystems/tty.md |
| Subsystem | Selftests | `tools/testing/selftests/` | subsystems/selftests.md |
| Subsystem | Wireless / mac80211 | `drivers/net/wireless/`, `net/mac80211/` | subsystems/wireless.md |
| Subsystem | Bluetooth | `net/bluetooth/`, `hci_*` | subsystems/bluetooth.md |
| Subsystem | SunRPC | `net/sunrpc/` | subsystems/sunrpc.md |
