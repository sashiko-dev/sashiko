+++
title = "DRM/GPU"
paths = ["drivers/gpu/drm/", "include/drm/", "include/uapi/drm/"]
symbols = ["drm_atomic_*", "drm_crtc_*", "amdgpu_*", "amdkfd*", "hwseq*", "hw_sequencer*"]
+++
# DRM/GPU: maintainer notes

Conventions and review judgment calls for DRM/KMS and GPU driver patches.

- `mdelay()` works in atomic context, but avoid it for long delays.
- When the driver picks a scaling mode itself (userspace has not asked for one), the default should keep the aspect ratio (`RMX_ASPECT`). Use `RMX_FULL`, which stretches the image, only when userspace explicitly requests it or a documented hardware limit requires it.
- Intel i915/xe: if new hardware shares a platform but differs from it (different IP version, PHY, PCH or display setup), model it as a subplatform. Put its PCI IDs in a separate macro (e.g. `INTEL_WCL_IDS` apart from `INTEL_PTL_IDS`), register the subplatform, and handle it the same way in i915 and xe. Do not just add the IDs to the existing platform macro.
- Commit messages and comments sometimes claim hardware facts such as "platform X doesn't have Y" or "this will never happen". Treat these as hypotheses to check, not facts to accept, especially when one platform flag covers several hardware variants.
- If a patch changes which DPCD register is used for probing or quirks, it should explain why the new register is safe to read in every operating context.
