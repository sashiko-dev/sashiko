# Sashiko: Guide for Kernel Maintainers

Welcome to the Sashiko guide for Linux Kernel Maintainers. This document outlines how you can interact with Sashiko and configure its behavior for your mailing lists.

## Support & Contact

If you have questions, feedback, or need assistance, please reach out through the following channels:

*   **GitHub Issues:** Use our issue tracker for bug reports, unexpected AI behavior, and feature requests.
*   **GitHub Pull Requests:** For contributing code, adjusting tracking configurations, or improving prompt templates.
*   **Mailing List:** Contact the Sashiko development mailing list at `sashiko@lists.linux.dev` for general discussions and inquiries. Automated patch reviews and bot communications use `sashiko-reviews@lists.linux.dev`.

## Tracking a New Mailing List

To request tracking for a new lore or NNTP mailing list, you can either:

1.  **Submit a Pull Request:** Add a `["<list-email>"]` section to [`projects/linux/mailing_lists.toml`](projects/linux/mailing_lists.toml). Sashiko automatically derives the Lore NNTP group name by reversing the domain components and appending the list's local part (e.g., `bpf@vger.kernel.org` becomes `org.kernel.vger.bpf`). If the NNTP group name on `nntp.lore.kernel.org` does not follow this convention, set `nntp_group = "..."` explicitly in the section.
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

## Configuring Mailing List & Email Delivery Options

All per-mailing-list ingestion, embargo, email delivery, and Patchwork settings for Linux live in [`projects/linux/mailing_lists.toml`](projects/linux/mailing_lists.toml). Each section key is the mailing list's email address (e.g. `["bpf@vger.kernel.org"]`), and omitted fields inherit from `[defaults]`:

*   **`track`:** Whether Sashiko ingests patches from this list via NNTP (default: `true`). Set `track = false` to configure delivery rules for an external list without polling NNTP for it.
*   **`nntp_group`:** Optional explicit Lore NNTP group override when the group name cannot be derived from the email address (e.g., `devicetree@vger.kernel.org` -> `org.kernel.vger.linux-devicetree`).
*   **`reply_to`:** List of recipient groups to include on review emails (`"author"`, `"list"`, `"recipients"`). Defaults to `[]` (tracked for the Web UI only, with no outbound emails sent unless `reply_to` or `cc` is configured). Set `reply_to = ["author"]` to send reviews only to the patch author without pinging the public list, or `reply_to = ["author", "list", "recipients"]` to reply to the public list and CC'd individuals.
*   **`positive_review`:** Who receives a review email when 0 issues are found: `"none"` (default), `"author"` (send positive confirmation only to the patch author), or `"all"` (use the normal `reply_to` and `cc` recipients).
*   **`mute_all`:** Completely mutes Sashiko for the mailing list, preventing any review emails from being sent.
*   **`cc`:** Static email addresses that should always receive a copy of the review.
*   **`ignored_emails`:** Author email addresses whose submissions will be muted.
*   **`embargo_hours`:** Hours to wait before publishing a review with findings (default: `0`). Clean reviews are released immediately after the complete patchset review succeeds.
*   **`subject_prefixes`:** Optional subject prefix tags (e.g. `["net", "net-next"]`) used to disambiguate `embargo_hours` when a patch is cross-posted to multiple mailing lists.

**Web UI tracking only (default):** Adding an empty section `["amd-gfx@lists.freedesktop.org"]` tracks and reviews patches from the list for the Web UI without sending outbound emails (`reply_to = []`):

```toml
["amd-gfx@lists.freedesktop.org"]
```

**Author-only delivery:** Set `reply_to = ["author"]` to send reviews with findings directly to the patch author without pinging the public mailing list:

```toml
["intel-xe@lists.freedesktop.org"]
reply_to = ["author"]
```

**Public list delivery:**

```toml
["bpf@vger.kernel.org"]
embargo_hours = 0
subject_prefixes = ["bpf", "bpf-next"]
reply_to = ["author", "list", "recipients"]
```

**Configuration:** To request a change to your mailing list configuration in [`projects/linux/mailing_lists.toml`](projects/linux/mailing_lists.toml), please open a GitHub Pull Request or Issue, or email the development mailing list (`sashiko@lists.linux.dev`).
