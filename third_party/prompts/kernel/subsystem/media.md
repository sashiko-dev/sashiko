# Media/V4L2 Subsystem Details

## V4L2 Subdevice State Validation Wrappers

Treating a core-validated V4L2 subdevice callback like an arbitrary C
function produces false NULL-dereference reports. The opposite mistake is
also dangerous: callbacks reached outside the validating wrapper, or using
different arguments, can still dereference a NULL state object.

- `v4l2_subdev_state_get_format()` can return `NULL`. Do not infer safety from
  the helper name or suppress checks of its result globally.
- For state-aware pad operations, inspect the real invocation path through
  `v4l2_subdev_call()` and the wrappers in
  `drivers/media/v4l2-core/v4l2-subdev.c`. In particular,
  `call_enum_mbus_code()` invokes `check_state()` before
  `sd->ops->pad->enum_mbus_code()`, while `call_get_fmt()` and
  `call_set_fmt()` reach the same validation through `check_format()`.
- For a subdevice with `V4L2_SUBDEV_FL_STREAMS`, `check_state()` calls
  `v4l2_subdev_state_get_format(state, pad, stream)` under
  `CONFIG_VIDEO_V4L2_SUBDEV_API`. It returns `-EINVAL` when that exact lookup
  returns `NULL`. Without `CONFIG_VIDEO_V4L2_SUBDEV_API`, it rejects the
  streams operation instead. A driver callback reached through this wrapper
  can therefore reuse the checked result for the same `state`, `pad`, and
  `stream` without another NULL check.
- Other state-aware wrappers, including enum frame size/interval and
  get/set selection, also use `check_state()`. Verify the operation-specific
  wrapper rather than extrapolating from one callback family to another.

Before dismissing a possible NULL dereference in a V4L2 subdevice callback,
establish all of the following from concrete code:

1. Resolve the callback's registration in its `v4l2_subdev_*_ops` table and
   identify every real caller, including calls through function pointers.
2. Locate the matching media-core `call_*` wrapper and prove that
   `check_state()` necessarily succeeds before the callback is invoked.
3. Match the checked `state`, `pad`, and `stream` expressions to the later
   lookup. Also match the callback operation and relevant compile-time and
   runtime conditions, including `CONFIG_VIDEO_V4L2_SUBDEV_API` and
   `V4L2_SUBDEV_FL_STREAMS`.
4. Cite the wrapper and `check_state()` as the evidence that disproves the
   candidate finding. Do not dismiss it merely because callers normally use
   the wrapper.

Keep the concern when any valid caller invokes the driver callback directly,
when a second caller bypasses the guard, when the guard is conditional, when
the callback uses a different state/pad/stream combination, or when the
available source context is insufficient to prove the complete path.

This boundary must survive consolidation and final verification. If the
analysis confirms a direct caller, a bypassing caller, or a mismatched lookup
and finds no concrete code that disproves that path, the final output must
retain the concern. An empty findings list would contradict that verification;
do not silently drop the concern after confirming its execution path.

## Related State Lookup Helpers

The state check proves only the exact lookup it performs. It does not prove
that every pointer derived from the state is non-NULL.

- `v4l2_subdev_state_get_opposite_stream_format()` performs a different
  routing lookup and may still need an explicit NULL check.
- Initialization callbacks and direct in-kernel invocations must be assessed
  from their own entry paths; do not transfer an ioctl-wrapper precondition
  to them without proof.
- A check for one pad or stream does not validate an arithmetic variant, an
  opposite endpoint, or values loaded from another request object.

## Quick Checks

- **Function-pointer callers**: follow the operation table to the media-core
  wrapper; a textual search for the driver callback name alone is incomplete.
- **Same-value proof**: compare the exact state, pad, and stream expressions
  on both sides of the callback boundary.
- **Universal reachability**: one guarded caller does not make a callback safe
  if another reachable caller bypasses the guard.
- **Missing context**: retain uncertainty instead of inventing a wrapper or
  precondition that is not present in the inspected source.
