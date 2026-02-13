# Power Domain Subsystem Details

## genpd stay_on and sync_state Interaction

Domains that are powered on at `pm_genpd_init()` time stay powered indefinitely
until `sync_state` fires, unless `GENPD_FLAG_NO_STAY_ON` is set. If consumers
never probe, `sync_state` never fires (in the default strict mode), and the
domain stays on forever -- wasting power and potentially blocking regulator
cleanup.

When `CONFIG_PM_GENERIC_DOMAINS_OF` is enabled and a domain is initialized as
on (`is_off=false`), `pm_genpd_init()` in `drivers/pmdomain/core.c` calls
`genpd_set_stay_on()`, which sets `genpd->stay_on = true` unless
`GENPD_FLAG_NO_STAY_ON` is set. While `stay_on` is true, `genpd_power_off()`
refuses to power off the domain (checked at the top of that function).

The `stay_on` flag is cleared when the provider's `sync_state` callback runs.
For OF-based providers, this is handled by `genpd_provider_sync_state()` or
`of_genpd_sync_state()`, both in `drivers/pmdomain/core.c`. These functions
set `genpd->stay_on = false` and then attempt `genpd_power_off()`.

The `sync_state` callback is only invoked after all consumer device links
reach `DL_STATE_ACTIVE` (i.e., all consumers have probed). If a consumer
never probes (no driver, deferred indefinitely, etc.), `sync_state` never
fires in strict mode. The `fw_devlink.sync_state=timeout` option (or
`CONFIG_FW_DEVLINK_SYNC_STATE_TIMEOUT`) changes this behavior to give up
waiting after `deferred_probe_timeout` expires or at `late_initcall()` if
`!CONFIG_MODULES`.

`GENPD_FLAG_NO_STAY_ON` prevents `stay_on` from being set at all, allowing
the domain to be powered off as soon as it has no active consumers -- without
waiting for `sync_state`. Platforms using this flag include:

- Renesas R-Car: `drivers/pmdomain/renesas/rcar-sysc.c`
- Renesas R-Mobile: `drivers/pmdomain/renesas/rmobile-sysc.c`
- Rockchip: `drivers/pmdomain/rockchip/pm-domains.c`
- Tegra BPMP: `drivers/pmdomain/tegra/powergate-bpmp.c`

Without `CONFIG_PM_GENERIC_DOMAINS_OF`, `genpd_set_stay_on()` unconditionally
sets `stay_on = false`, so the stay-on mechanism is inactive.

## genpd_power_off_unused and Regulator Cleanup Ordering

If `genpd_power_off_unused()` is delayed (e.g., by the `stay_on` mechanism),
domains remain powered past the point where regulators expect them to be off.
On platforms where a regulator supplies a power domain, this can cause the
regulator to be disabled while its domain is still active, leading to hardware
malfunction.

`genpd_power_off_unused()` runs at `late_initcall_sync`
(`drivers/pmdomain/core.c`). It iterates all registered genpds and queues
`genpd_power_off_work` for each, which asynchronously attempts to power off
domains with no active consumers. Domains with `stay_on == true` are skipped.

`regulator_init_complete()` also runs at `late_initcall_sync`
(`drivers/regulator/core.c`), but it does not disable regulators
synchronously. It schedules `regulator_init_complete_work` as a delayed work
with a 30-second timeout, which eventually calls `regulator_late_cleanup()`
to disable unused regulators.

Even with the 30-second delay, if domains are held on by `stay_on` awaiting
a `sync_state` callback that never arrives, the domain may still be powered
when the regulator is finally disabled. The Rockchip power domain driver sets
`GENPD_FLAG_NO_STAY_ON` specifically to avoid this scenario.

## Platform-Specific Workaround Conditionals

Applying workarounds unconditionally causes unnecessary code execution on
unaffected platforms and may introduce regressions on hardware that does not
need the workaround.

Workarounds for bootloader state handover (e.g., splash-screen handover)
must be gated on the affected platform:

- Architecture checks (e.g., `IS_ENABLED(CONFIG_ARM)` for ARM32-only issues)
- Device compatibility checks (`of_device_is_compatible()`)
- Platform-specific device tree properties

Timing also matters: power domain reset operations for bootloader handover
should occur early in probe, before `pm_genpd_init()` initializes the
domain. After `pm_genpd_init()`, the domain is registered in the global
`gpd_list` and managed by the genpd framework, so manual reset may conflict
with framework state.

For targeted resets, explicit per-driver power-off functions (e.g.,
`exynos_pd_power_off()` in `drivers/pmdomain/samsung/exynos-pm-domains.c`)
are preferred over `of_genpd_sync_state()`, which iterates all domains
belonging to a provider and attempts to power off each one.

## Platform Default Domain States

Forcing domains on at boot when they should default to off wastes power and
may violate hardware constraints.

Each vendor has its own mechanism for expressing default-off state:

- MediaTek uses `MTK_SCPD_KEEP_DEFAULT_OFF` (defined in
  `drivers/pmdomain/mediatek/mtk-pm-domains.h`) in per-domain capability
  flags. When set, the domain is initialized as off via `pm_genpd_init()`
  with `is_off=true`.
- Renesas and Rockchip use `GENPD_FLAG_NO_STAY_ON` to allow domains to
  power off without waiting for `sync_state`.

Core genpd changes that alter default on/off behavior must be verified
against platform drivers that rely on these mechanisms.

## Quick Checks

- **`of_genpd_sync_state()` as sync_state callback**: This function iterates
  all domains belonging to a provider and powers off each one. If a platform
  driver uses it as a `sync_state` callback, verify this is appropriate for
  all domains the provider manages, or whether only specific domains should
  be powered off.
- **`GENPD_FLAG_NO_STAY_ON` on new drivers**: Drivers for platforms with
  regulator-supplied power domains or unreliable `sync_state` invocation
  should set this flag.
- **Core genpd timing changes**: New default behavior that delays or prevents
  domain power-off must provide a `GENPD_FLAG_*` opt-out for platforms that
  cannot tolerate the change.
