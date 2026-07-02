# Subsystem Guide Index

Load subsystem guides from the prompt directory based on what the code
touches. Each guide contains subsystem-specific invariants, API
contracts, and common bug patterns. Each subsystem guide may reference
additional pattern files to load conditionally.

A change can match multiple rows. Load **every** matching guide, not
just the deepest or most specific. For example, code touching
`arch/arm64/kvm/hyp/` matches the ARM64, KVM, ARM64 KVM (EL1/Host),
and ARM64 Hyp (EL2) rows — all four guides apply.

The triggers column below includes both path names, function calls,
and symbols regexes.

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| Networking Core | net/, skb_, sockets, xfrm, dst_, sock_put, release_sock, pskb_may_pull, SNMP_*_STATS | networking-core.md |
| Networking Drivers | drivers/net/, ethtool_ops, net_device_ops | networking-drivers.md |
| Netlink | `genl_`, `nla_`, `NLA_`, `NLM_F_`, `nlmsg_`, `netlink_callback`, Documentation/netlink/specs/, files marked `YNL-GEN` | netlink.md |
| MM Page Tables | `pte_*`, `pmd_*`, `pud_*`, `set_pte`, `ptep_*`, `tlb_*`, `page_vma_mapped_walk`, `walk_page_range`, `zap_pte_range`, mm/memory.c, mm/mprotect.c, mm/pagewalk.c | mm-pagetable.md |
| Alignment Helpers | `ALIGN`, `ALIGN_DOWN`, `IS_ALIGNED`, `PAGE_ALIGN`, `PAGE_ALIGN_DOWN`, `pageblock_align`, `pageblock_aligned`, `pageblock_start_pfn`, `pageblock_end_pfn` | alignment.md |
| MM Folio/Page Cache | `folio_*`, `page_folio`, `compound_head`, `filemap_*`, `xa_*`, `xas_*`, `page_cache_*`, mm/filemap.c, mm/swap.c, mm/truncate.c | mm-folio.md |
| MM Large Folios/THP/Hugetlb | `huge_memory`, `hugetlb`, `split_huge_*`, `folio_test_large`, `hstate`, PMD sharing, mm/huge_memory.c, mm/hugetlb.c, mm/memory-failure.c | mm-largepage.md |
| MM VMA Operations | `vma_*`, `mmap_*`, `vm_area_struct`, `vm_flags`, `anon_vma`, `maple_tree`, mm/vma.c, mm/mmap.c, mm/mmap_lock.c | mm-vma.md |
| MM Allocation | `alloc_pages`, `__GFP_*`, `kmalloc`, `kmem_cache_*`, `slub`, `vmalloc`, `zone_watermark`, `mempool`, `memblock`, mm/page_alloc.c, mm/slub.c, mm/vmalloc.c | mm-alloc.md |
| MM Reclaim/Swap/Migration | `vmscan`, `shrink_*`, `lru_*`, `swap_*`, `shmem_*`, `mem_cgroup_*`, `writeback`, `migrate_*`, mm/vmscan.c, mm/swap_state.c, mm/migrate.c, mm/memcontrol.c | mm-reclaim.md |
| VFS | inode, dentry, vfs_, fs/*.c | vfs.md |
| Locking | spin_lock*, mutex_*, rwsem*, seqlock*, *seqcount* | locking.md |
| Scheduler | kernel/sched/, sched_, schedule, *wakeup* | scheduler.md |
| Timers | timer_list, timer_setup, mod_timer, del_timer, hrtimer, delayed_work | timers.md |
| BPF | kernel/bpf/, tools/lib/bpf/, tools/testing/selftests/bpf, bpf, verifier | bpf.md |
| BTF Fields | `map_check_btf`, `check_and_init_map_value`, `bpf_obj_free_fields`, `BPF_SPIN_LOCK`, `BPF_TIMER`, `BPF_KPTR` | btf.md |
| Libbpf API | tools/lib/bpf/, `LIBBPF_API`, `libbpf_err`, `libbpf_err_ptr` | libbpf.md |
| RCU | rcu*, call_rcu, synchronize_rcu, kfree_rcu, kvfree_call_rcu | rcu.md |
| Encryption | crypto, fscrypt_ | fscrypt.md |
| Tracing | trace_, tracepoints | tracing.md |
| Workqueue | kernel/workqueue.c, work_struct | workqueue.md |
| Syscalls | `SYSCALL_DEFINE`, `copy_from_user`, `copy_to_user`, `get_user`, `put_user`, any change to syscall parameter validation | syscall.md |
| btrfs | fs/btrfs/ | btrfs.md |
| DAX | dax operations | dax.md |
| Block/NVMe | block layer, nvme | block.md |
| DRM/GPU | drivers/gpu/drm/, drm_atomic_, drm_crtc_, hwseq, hw_sequencer | drm.md |
| NFSD | fs/nfsd/*, fs/lockd/* | nfsd.md |
| SunRPC | net/sunrpc/* | sunrpc.md |
| io_uring | io_uring/, io_uring_, io_ring_, io_sq_, io_cq_, io_wq_, IORING_ | io_uring.md |
| Cleanup API | `__free`, `guard(`, `scoped_guard`, `DEFINE_FREE`, `DEFINE_GUARD`, `no_free_ptr`, `return_ptr` | cleanup.md |
| RCU lifecycle | `call_rcu(`, `kfree_rcu(`, `synchronize_rcu(`, `rhashtable_*` + `call_rcu`, `hlist_del_rcu` + `call_rcu`, `list_del_rcu` + `call_rcu` | rcu.md |
| Power Domains | drivers/pmdomain/, pm_genpd_, of_genpd_, exynos_pd_ | pmdomain.md |
| PM Runtime | include/linux/pm_runtime.h, pm_runtime_, __pm_runtime_, rpm_idle, rpm_suspend, rpm_resume | pm.md |
| Sysfs | fs/sysfs/, sysfs_create_group, sysfs_update_group, attribute_group, is_visible | sysfs.md |
| CXL | drivers/cxl/, cxl_, hmat_get_extended_linear_cache_size | cxl.md |
| Bluetooth | net/bluetooth/, hci_, HCI_LE_ADV, adv_instances, cur_adv_instance | bluetooth.md |
| TTY/Serial | drivers/tty/, uart_add_one_port, uart_ops, serial_core | tty.md |
| PCI | drivers/pci/, pci_epc_, pci_epf_, pci_ep_ | pci.md |
| SMB/ksmbd | fs/smb/server/, ksmbd_, smb_direct_ | smb-ksmbd.md |
| Open Firmware (DT) | drivers/of/, of_node, of_find_, of_get_, of_parse_, for_each_child_of_node, for_each_available_child_of_node, of_node_put, of_node_get | of.md |
| Perf Tools | tools/perf/, openat, fdopendir, closedir | perf.md |
| Multi-Function Devices (MFD) | drivers/mfd/, include/linux/mfd/, mfd_add_devices, devm_mfd_add_devices, mfd_cell, mfd_remove_devices | mfd.md |
| MIPS | arch/mips/, tlb_probe, tlb_read, tlb_write_indexed, write_c0_entryhi, read_c0_index, TLBP, TLBR, TLBWI | mips.md |
| hwmon | drivers/hwmon/, hwmon_*, asus-ec-sensors, ec_board_info | hwmon.md |
| Wireless/mac80211 | drivers/net/wireless/, net/mac80211/, BSS_CHANGED_, vif_cfg_changed, link_info_changed, bss_info_changed | wireless.md |
| Selftests | tools/testing/selftests/, TEST_PROGS, TEST_FILES, TEST_GEN_FILES | selftests.md |
| DT Bindings | Documentation/devicetree/bindings/, *.yaml in devicetree | dt-bindings.md |
| USB Storage | drivers/usb/storage/, unusual_devs.h, UNUSUAL_DEV, USB_SC_, USB_PR_ | usb-storage.md |
| ATA/libata | drivers/ata/, ata_dev_, ata_port_, ata_read_log_, ATA_QUIRK_ | ata.md |
| I/O Accessors | writesl, readsl, writesw, readsw, writesb, readsb, __raw_writel, __raw_readl, FIFO | io-accessors.md |
| Kconfig | Kconfig, `config `, `select `, `depends on `, `tristate `, `bool ` | kconfig.md |
| Build System | Kbuild, Makefile, scripts/, tools/, `gnu11`, `-funsigned-char`, `-fno-strict-aliasing` | build.md |
| Input | drivers/input/, include/linux/input.h, include/linux/input/, input_dev, input_handler, input_register_, input_report_  | input.md |
| Objtool | tools/objtool/, INSN_BUG, INSN_TRAP, decode.c | objtool.md |
| KHO (Kexec Handover) | lib/test_kho.c, kho_, kho_is_enabled, kho_retrieve_subtree, kho_preserve_folio, kho_add_subtree, register_kho_notifier | kho.md |
| I2C | drivers/i2c/, i2c_*, | i2c.md |
| Rust | any Rust code | rust.md |
| KVM | virt/kvm/, include/linux/kvm*, kvm_ | kvm.md |
| ARM64 | arch/arm64/, sysreg | arm64.md |
| ARM64 KVM (EL1/Host) | arch/arm64/kvm/ | kvm-arm64.md |
| ARM64 Hyp (EL2) | arch/arm64/kvm/hyp/, __hyp_, arch/arm64/include/asm/kvm.*\.h, drivers/iommu/arm/arm-smmu-v3/pkvm/ | hyp-arm64.md |

## Optional Patterns

Load only when explicitly requested in the prompt:

- **Subjective Review** (subjective-review.md): Subjective general assessment
