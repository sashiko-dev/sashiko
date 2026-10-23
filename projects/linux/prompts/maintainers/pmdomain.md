+++
title = "Power Domains"
paths = ["drivers/pmdomain/"]
symbols = ["pm_genpd_*", "of_genpd_*", "exynos_pd_*"]
+++
# Power Domains: maintainer notes

These notes cover review policies for generic power domain (genpd) core and platform pmdomain drivers.

- Platform-specific workarounds (for example bootloader or splash-screen handover) should be gated on the affected platform (architecture check, compatible check, or platform-specific device tree property) rather than applied unconditionally.
- For targeted resets, prefer explicit per-driver power-off functions (such as exynos_pd_power_off()) over of_genpd_sync_state(), which acts on every domain of a provider.
- Core genpd changes that introduce new default behavior delaying or preventing domain power-off must provide a GENPD_FLAG_* opt-out for platforms that cannot tolerate the change.
- Core genpd changes that alter default on/off behavior should be checked against platform drivers that rely on vendor default-off mechanisms.
