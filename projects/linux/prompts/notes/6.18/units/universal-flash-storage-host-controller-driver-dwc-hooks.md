# UNIVERSAL FLASH STORAGE HOST CONTROLLER DRIVER DWC HOOKS

Provides Synopsys DesignWare Core (DWC) UFS host controller hooks and glue drivers for the Synopsys G210 Test Chip over PCI and platform (Device Tree) buses, implementing UFSHCD variant operations for link startup notification, clock divider programming, UniPro connection setup, and 20-bit/40-bit RMMI M-PHY DME attribute configuration.

## Architecture & Data Flow

- `tc_dwc_g210_pci_probe` and `tc_dwc_g210_pltfm_probe` must not overwrite device driver data after `ufshcd_init` or `ufshcd_pltfrm_init` sets it to `struct ufs_hba *`, because `tc_dwc_g210_pci_remove`, `tc_dwc_g210_pltfm_remove`, and `ufshcd_*` PM ops cast driver data directly to `struct ufs_hba *`. Broken by: passing a glue-private pointer to `pci_set_drvdata`, `platform_set_drvdata`, or `dev_set_drvdata`. [F32]
- In `enum selector_index`, M-PHY lane selector indices passed to `UIC_ARG_MIB_SEL` are sparse: TX lanes 0 and 1 use `SELIND_LN0_TX` (`0x00`) and `SELIND_LN1_TX` (`0x01`), whereas RX lanes 0 and 1 start at `0x04` (`SELIND_LN0_RX = 0x04`, `SELIND_LN1_RX = 0x05`). Broken by: computing RX lane selector indices as 0-based (0, 1) or contiguous (2, 3) values. [F22]
- In `ufshcd_dwc_connection_setup`, both `DME_LOCAL` and `DME_PEER` UniPro CPort sequences in `setup_attrs` must set `T_CONNECTIONSTATE` to `0` before modifying device ID and CPort attributes, and set `T_CONNECTIONSTATE` back to `1` only after those attributes are programmed. [F17]
- Every non-sentinel entry in `tc_dwc_g210_pltfm_match` must populate `.data` with a valid `struct ufs_hba_variant_ops` pointer, and `tc_dwc_g210_pltfm_probe` requires the device to match `tc_dwc_g210_pltfm_match` because it unconditionally dereferences `of_id->data`. Broken by: adding a match entry without `.data` or binding to a non-OF platform device without guarding `of_match_node()` for `NULL`. [F37]
- Device match tables `tc_dwc_g210_pci_tbl` and `tc_dwc_g210_pltfm_match` must be terminated by an empty `{ }` sentinel element. [F23]

## Synchronization

- `ufshcd_dwc_dme_set_attrs`, `ufshcd_dwc_link_startup_notify`, `tc_dwc_g210_config_40_bit`, and `tc_dwc_g210_config_20_bit` must only be called in sleepable process context without holding `hba->uic_cmd_mutex` or any spinlock, because they call `ufshcd_dme_set_attr`, `ufshcd_dme_set`, and `ufshcd_dme_get`, which lock `hba->uic_cmd_mutex` and sleep. [F41]
- In `tc_dwc_g210_pci_probe`, `tc_dwc_g210_pci_hba_vops.phy_initialization` and `hba->vops` must be populated before calling `ufshcd_init`, because `ufshcd_init` synchronously invokes `hba->vops->link_startup_notify` (which calls `hba->vops->phy_initialization`) and publishes the SCSI host. [F5]

## Object Lifetimes

- Because `tc_dwc_g210_pltfm_probe` initializes the host and enables runtime PM via `ufshcd_pltfrm_init(pdev, vops)`, `tc_dwc_g210_pltfm_remove` must tear down the device via `ufshcd_pltfrm_remove(pdev)` rather than calling `ufshcd_remove` directly without balancing runtime PM. [F4]
- `tc_dwc_g210_pci_probe` must call `pm_runtime_put_noidle(&pdev->dev)` and `pm_runtime_allow(&pdev->dev)` only after `ufshcd_init` succeeds, and `tc_dwc_g210_pci_remove` must reverse this in LIFO order via `pm_runtime_forbid(&pdev->dev)` and `pm_runtime_get_noresume(&pdev->dev)` before calling `ufshcd_remove(hba)`. [F2]
- Because `tc_dwc_g210_pci_probe` acquires PCI enablement, BAR 0 MMIO mappings, and the UFS host via devres helpers (`pcim_enable_device`, `pcim_iomap_region`, `ufshcd_alloc_host`), error paths and `tc_dwc_g210_pci_remove` must only call `ufshcd_remove` for active hosts and never manually call `pci_disable_device`, `pci_iounmap`, or `scsi_host_put`. [F34]

## API Contracts

- Because the UFSHCD core never invokes `struct ufs_hba_variant_ops::phy_initialization` directly, any DWC `ufs_hba_variant_ops` table that sets `.phy_initialization` must also wire `.link_startup_notify = ufshcd_dwc_link_startup_notify`, which calls `ufshcd_vops_phy_initialization(hba)` during `PRE_CHANGE`. [F43]
- In `ufshcd_dwc_link_startup_notify`, the `PRE_CHANGE` phase must program `DWC_UFS_REG_HCLKDIV` and run `ufshcd_vops_phy_initialization(hba)`, whereas the `POST_CHANGE` phase must verify the link via `ufshcd_dwc_link_is_up(hba)` (which calls `ufshcd_set_link_active(hba)` when `VS_POWERSTATE` equals `UFSHCD_LINK_IS_UP`) before running `ufshcd_dwc_connection_setup(hba)`. [F38]
- Every `struct ufshcd_dme_attr_val` entry passed to `ufshcd_dwc_dme_set_attrs` must encode `attr_sel` using `UIC_ARG_MIB_SEL(attr, sel)` or `UIC_ARG_MIB(attr)` (placing the 16-bit attribute ID in bits `[31:16]` and selector index in bits `[15:0]`), and callers must pass the element count (`ARRAY_SIZE(...)`, not `sizeof(...)`) as parameter `n`. [F12, F30]
- In `tc_dwc_g210_config_40_bit` and `tc_dwc_g210_config_20_bit`, after programming M-PHY RMMI shadow registers via `tc_dwc_g210_setup_40bit_rmmi` or `tc_dwc_g210_setup_20bit_rmmi`, the driver must write `0x01` to `UIC_ARG_MIB(VS_MPHYCFGUPDT)` via `ufshcd_dme_set` to latch the shadow bank before writing `UIC_ARG_MIB(VS_DEBUGOMC)`. [F39]
- `tc_dwc_g210_setup_20bit_rmmi_lane1` must query `PA_AVAILRXDATALANES` and `PA_AVAILTXDATALANES` via `ufshcd_dme_get` and only program Lane 1 TX attributes (`setup_tx_attrs`) when `connected_tx_lanes == 2` and Lane 1 RX attributes (`setup_rx_attrs`) when `connected_rx_lanes == 2`. [F40]
- In `tc_dwc_g210_pci_probe`, `pcim_enable_device(pdev)`, `pci_set_master(pdev)`, and `pcim_iomap_region(pdev, 0, UFSHCD)` must complete before `ufshcd_init(hba, mmio_base, pdev->irq)` is called. [F42]

## Error Handling

- In `tc_dwc_g210_pci_probe`, the `mmio_base` pointer returned by `pcim_iomap_region` must be checked with `IS_ERR` and converted with `PTR_ERR` before calling `ufshcd_init`, because `pcim_iomap_region` returns an `ERR_PTR` on failure while `ufshcd_init` only checks `!mmio_base`. [F11]
- In `ufshcd_dwc_link_is_up` and `tc_dwc_g210_setup_20bit_rmmi_lane1`, local out-parameters passed to `ufshcd_dme_get` (`dme_result`, `connected_rx_lanes`, `connected_tx_lanes`) must be initialized before the call unless the return value of `ufshcd_dme_get` is checked, because `ufshcd_dme_get_attr` does not write `*mib_val` on error. [F9]
- In `tc_dwc_g210_setup_20bit_rmmi_lane1`, `ret` must be initialized to `0` before the `connected_tx_lanes == 2` and `connected_rx_lanes == 2` branches because 1-lane configurations skip both branches and return `ret` directly. [F10]
- `ufshcd_dwc_dme_set_attrs` iterates sequentially from index `0` to `n - 1` and must abort immediately and return the non-zero error code on the first failed `ufshcd_dme_set_attr` call. [F26]

## External Contracts

- `tc_dwc_g210_pci_probe` must validate the `tc_type` module parameter against `TC_G210_20BIT` (`20`) and `TC_G210_40BIT` (`40`) and return `-EPERM` on any other value before enabling the PCI device or registering `tc_dwc_g210_pci_hba_vops`, because `tc_type` defaults to `TC_G210_INV` (`0`), which would leave `phy_initialization` unset. [F20]

## Known Pitfalls

- `tc_dwc_g210_pci_probe` assigns the address of the single mutable file-scope `tc_dwc_g210_pci_hba_vops` structure to `hba->vops`, so `tc_dwc_g210_pci_hba_vops` must not be mutated with per-device differences (such as based on `id->device`) without first splitting it into separate `ufs_hba_variant_ops` instances as in `tc-dwc-g210-pltfrm.c`. [F18]
