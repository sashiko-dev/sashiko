# Subsystem Guide Index

Load subsystem guides from the prompt directory based on what the code touches.
Each guide contains subsystem-specific invariants, API contracts, and common
bug patterns. Each subsystem guide may reference additional pattern files to
load conditionally.

The triggers column below includes both path names, function calls, and symbols
regexes

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| Networking | net/, drivers/net/, skb_, sockets | networking.md |
| Memory Management | mm/, page/folio ops, alloc/free, slab, vmalloc, `__GFP_*`, `page_*`, `folio_*`, `kmalloc`, `kmem_cache_*`, `vmalloc`, `alloc_pages` | mm.md |
| VFS | inode, dentry, vfs_, fs/*.c | vfs.md |
| Locking | spin_lock*, mutex_*, rwsem*, seqlock*, *seqcount* | locking.md |
| Scheduler | kernel/sched/, sched_, schedule, *wakeup* | scheduler.md |
| BPF | kernel/bpf/, bpf, verifier | bpf.md |
| RCU | rcu*, call_rcu, synchronize_rcu, kfree_rcu | rcu.md |
| Encryption | crypto, fscrypt_ | fscrypt.md |
| Tracing | trace_, tracepoints | tracing.md |
| Workqueue | kernel/workqueue.c, work_struct | workqueue.md |
| Syscalls | syscall definitions | syscall.md |
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
| MIPS | arch/mips/, tlb_probe, tlb_read, tlb_write_indexed, write_c0_entryhi, read_c0_index, TLBP, TLBR, TLBWI | mips.md |
| hwmon | drivers/hwmon/, asus-ec-sensors, ec_board_info | hwmon.md |
| Media/Video | drivers/media/, v4l2_, V4L2_PIX_FMT_, iris_, video_device | media.md |
| Wireless/mac80211 | drivers/net/wireless/, net/mac80211/, BSS_CHANGED_, vif_cfg_changed, link_info_changed, bss_info_changed | wireless.md |
| Selftests | tools/testing/selftests/, TEST_PROGS, TEST_FILES, TEST_GEN_FILES | selftests.md |
| IRQ Chip | drivers/irqchip/, gic_, its_, irq_chip, irq_domain | irqchip.md |
| CAN | drivers/net/can/, can_, canfd_, rcar_canfd, socketcan | can.md |
| DT Bindings | Documentation/devicetree/bindings/, *.yaml in devicetree | dt-bindings.md |
| USB Storage | drivers/usb/storage/, unusual_devs.h, UNUSUAL_DEV, USB_SC_, USB_PR_ | usb-storage.md |
| ATA/libata | drivers/ata/, ata_dev_, ata_port_, ata_read_log_, ATA_QUIRK_ | ata.md |
| I/O Accessors | writesl, readsl, writesw, readsw, writesb, readsb, __raw_writel, __raw_readl, FIFO | io-accessors.md |
| DPLL | drivers/dpll/, dpll_, zl3073x_, ZL_REG_, ZL_INFO_ | dpll.md |
| Kconfig | Kconfig, `config `, `select `, `depends on `, `tristate `, `bool ` | kconfig.md |
| I3C | drivers/i3c/, i3c_master_, i3c_device_, i2c_adapter, svc-i3c-master | i3c.md |
| Input | drivers/input/, edt-ft5x06, touchscreen@, report-rate-hz | input.md |
| Objtool | tools/objtool/, INSN_BUG, INSN_TRAP, decode.c | objtool.md |
| KHO (Kexec Handover) | lib/test_kho.c, kho_, kho_is_enabled, kho_retrieve_subtree, kho_preserve_folio, kho_add_subtree, register_kho_notifier | kho.md |

## Optional Patterns

Load only when explicitly requested in the prompt:

- **Subjective Review** (subjective-review.md): Subjective general assessment
