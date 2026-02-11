# Mirroring lore.kernel.org Archives

Sashiko supports an "Offline/Test Mode" that reads from local git clones of mailing list archives. This is useful for development, testing, and bulk import of historical data without stressing the live NNTP servers.

## Prerequisites

- `git` installed on your system.
- Sufficient disk space (LKML archives can be tens of gigabytes).

## Lore Epochs and Git Layout

`lore.kernel.org` (running `public-inbox`) splits large mailing lists into **epochs** (e.g., `0.git`, `1.git`, `2.git`...) to keep repository sizes manageable and performance high. This structure roughly follows standard [git repository layout](https://mirrors.edge.kernel.org/pub/software/scm/git/docs/gitrepository-layout.html) principles where large histories are segmented.

- **Epoch 0 (`0.git`)**: Contains the oldest messages (start of the archive).
- **Epoch N (`N.git`)**: Contains the newest messages.

Each epoch is a valid **bare** git repository. While they are often linked via `objects/info/alternates` on the server or in full mirrors to share common objects (like blobs), Sashiko treats them as individual sources of data during ingestion.

## Finding the Git URL

1.  Go to [lore.kernel.org](https://lore.kernel.org/).
2.  Navigate to the list you are interested in (e.g., `LKML`).
3.  Look for the "mirror" instructions.

## Cloning the Archive

### Simple Clone (Latest Messages)

To get the most recent messages, you should clone the **latest epoch**. Sashiko's ingestor automatically attempts to discover and clone the latest epoch if you provide the `--download` flag.

Manual example for LKML (assuming epoch 18 is latest):
```bash
# We use --bare as Sashiko treats these as bare repositories.
mkdir -p archives
cd archives
git clone --bare --depth=1000 https://lore.kernel.org/lkml/18.git archives/lkml/18.git
```

*Note: The actual URL structure requires checking the `manifest.js.gz` or the website to find the highest number.*

### Automatic Bootstrapping

Sashiko's ingestor (`src/ingestor.rs`) includes logic to:
1.  Fetch `https://lore.kernel.org/manifest.js.gz`.
2.  Find the highest numbered epoch for the requested list (e.g., `lkml`).
3.  Clone that epoch into `archives/<list>/<epoch>.git`.

### Using Grokmirror (Recommended for Full Mirrors)

For a robust, continuously updated mirror of the entire history, the kernel infrastructure team recommends `grokmirror`.

1.  Install `grokmirror`:
    ```bash
    pip install grokmirror
    ```

2.  Configure it to track specific lists. Create a `grokmirror.conf` (example):
    ```ini
    [core]
    toplevel = /path/to/sashiko/archives
    log = /path/to/sashiko/grokmirror.log

    [remote]
    site = https://lore.kernel.org
    manifest = https://lore.kernel.org/manifest.js.gz
    ```

3.  Run the pull command:
    ```bash
    grok-pull -c grokmirror.conf
    ```

## Sashiko Directory Structure

Sashiko expects archives to be placed in the `archives/` directory at the project root by default. This can be configured via `git.archives_dir` in `Settings.toml`.

```text
sashiko/
├── archives/
│   ├── lkml/
│   │   ├── 0.git/
│   │   ├── 1.git/
│   │   └── ...
│   └── netdev/
│       └── ...
└── ...
```

When running Sashiko in offline mode, point it to these directories.

## Helper Script

We plan to add a helper script in `scripts/mirror_lore.sh` to automate this process.

*(See `TODO.md` for status on automated tools)*
