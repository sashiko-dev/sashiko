# Sashiko: Guide for Kernel Maintainers

Welcome to the Sashiko guide for Linux Kernel Maintainers. This document outlines how you can interact with Sashiko and configure its behavior for your mailing lists.

## Support & Contact

If you have questions, feedback, or need assistance, please reach out through the following channels:

*   **GitHub Issues:** Use our issue tracker for bug reports, unexpected AI behavior, and feature requests.
*   **GitHub Pull Requests:** For contributing code, adjusting tracking configurations, or improving prompt templates.
*   **Mailing List:** Contact the Sashiko development mailing list at `sashiko@lists.linux.dev` for general discussions and inquiries. Automated patch reviews and bot communications use `sashiko-reviews@lists.linux.dev`.

## Tracking a New Mailing List

To request tracking for a new lore or NNTP mailing list, you can either:

1.  **Submit a Pull Request:** Directly update the tracking configuration by adding your lore or NNTP details to the `SASHIKO__MAILING_LISTS__TRACK` environment variable in [`sashiko.dev/base/app/sashiko-k8s.yaml`](sashiko.dev/base/app/sashiko-k8s.yaml).
2.  **Send an Email:** Contact the Sashiko development mailing list (`sashiko@lists.linux.dev`) and `Cc: Roman Gushchin <roman.gushchin@linux.dev>` with your request.

## Adding Subsystem-Specific Prompts

If you'd like to customize the review criteria or focus areas for your subsystem, you can provide subsystem-specific prompts. There are two ways to do this:

1.  **Submit a Pull Request to this Repository:** Add your prompt markdown file directly into the [`third_party/prompts/kernel/subsystem/`](third_party/prompts/kernel/subsystem/) directory.
2.  **Submit a Pull Request to Chris Mason's Repository:** Contribute your prompts upstream to [Chris Mason's repository](https://github.com/masoncl/review-prompts), which is periodically synced into Sashiko.

> **Note:** Please keep your prompts small and focused. Avoid adding trivial facts or generic programming advice, as this only wastes the AI's context window and can degrade review quality.

## How Sashiko Detects the Baseline for Your Patch

Before reviewing, Sashiko must determine which kernel tree and commit your patch is based on (the "baseline"). It builds an ordered list of candidate baselines and uses the first one the series applies cleanly against:

1.  **Explicit base commit** — the `base-commit:` trailer in the patch body.
2.  **Version tag** — e.g. `[PATCH 5.15.y]` maps to the `linux-5.15.y` stable branch or the local `v5.15` tag.
3.  **Subsystem tree** — the subsystem matched from `MAINTAINERS` (via its `F:` path patterns), using that entry's `T:` trees.
4.  **linux-next** — the aggregate `linux/linux-next` tree.
5.  **Mainline** — Linus/origin `master`.
6.  **Custom remotes** — trees you configure under `[git.custom_remotes]`.

### How `{subsystem}-next` trees are chosen

A subsystem's `T:` entries in `MAINTAINERS` point to the tree(s) Sashiko uses as baselines. Many subsystems maintain a `-next` tree (e.g. `net-next`, `perf-tools-next`) that carries in-flight work. Sashiko prefers the `-next` variant when the patch subject carries a `-next` prefix (e.g. `[PATCH net-next] ...`), since that signals the patch targets the `-next` branch.

If the subject does not carry `-next` and the subsystem lists both a stable and a `-next` tree, the stable tree is preferred. When the `-next` tree is the *only* `T:` entry for the subsystem, it is used regardless of the subject.

You can add additional baseline trees (and restrict to specific branches) with `[[git.custom_remotes]]` in your Sashiko config; see [`docs/configuration.md`](docs/configuration.md) for the schema.

## Configuring Email Delivery Options

Sashiko provides flexible delivery mechanisms that can be configured per mailing list or per individual maintainer.

*   **`reply_all`:** Controls whether Sashiko can reply directly to the public mailing list. If set to `false`, reviews are restricted to private recipients (the author and/or maintainers).
*   **`reply_to_author`:** Determines whether the review email should be sent directly to the author of the patch.
*   **`cc_individuals`:** Controls whether other maintainers and users in the CC list of the patch should be included in the review email.
*   **`mute_all`:** Completely mutes Sashiko for the given scope, preventing any review emails from being sent.
*   **`cc`:** A static list of email addresses that should always receive a copy of the review.
*   **`ignored_emails`:** A list of author email addresses whose submissions will never be reviewed by the system.
*   **`embargo_hours`:** The number of hours to wait before publishing a review with findings. Reviews with no findings are released immediately after the complete patchset review succeeds. When a patch is sent to multiple mailing lists, the shortest explicitly configured embargo period among the matched subsystems will be used. If no matched subsystems explicitly configure this value, it falls back to the default policy.
*   **`send_positive_review`**: Determines whether to send a review email even when no issues are found. If set to `true`, Sashiko will send a reply indicating that the review was positive. Defaults to `false`.

**Author-only delivery:** To *track* a subsystem's list (so its patches are
ingested and reviewed) without *pinging* the list, send the review only to
the patch author. No extra flag is needed — combine `reply_all = false`
(strips the list from recipients), `reply_to_author = true`, and
`cc_individuals = false`:

```toml
[subsystems.drm-intel]
lists = ["intel-xe@lists.freedesktop.org"]
reply_all = false
reply_to_author = true
cc_individuals = false
```

**Configuration:** The email policies and delivery preferences are defined in the [`sashiko.dev/email_policy.toml`](sashiko.dev/email_policy.toml) file. To request a change to your configuration, please open a GitHub Issue or email the development mailing list (`sashiko@lists.linux.dev`).
