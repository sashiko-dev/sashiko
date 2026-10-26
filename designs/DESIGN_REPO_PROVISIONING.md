# Provisioning the Repository Under Review

## Objective

Give every project one way to obtain and refresh the tree it reviews, owned
by the binary rather than by per-deployment shell scripts.

A daemon reads the tree from `[git] repository_path`. How that tree gets
there was decided separately in every place that runs a daemon:

| Where | Linux | Sashiko | GCC (proposed) |
|---|---|---|---|
| Built from source | `third_party/linux` submodule, later `make repo` | undocumented | undocumented |
| Container image | `scripts/docker-entrypoint.sh`: clone from `clone.bundle` in the background, mark `.ready` | ConfigMap script: plain clone, blocking | ConfigMap script: clone a GitHub mirror, re-point `origin`, fetch, blocking |
| `cargo install` | clone by hand | clone by hand | clone by hand |

Each script repeats the same steps (clone if missing, otherwise fetch, keep
`origin` pointed at the upstream) with small differences, the upstream URL of
each project lives in YAML or environment variables, and the binary has no
idea where its project's code comes from. When the tree is missing the daemon
starts anyway: MAINTAINERS indexing degrades to a warning and the first sign
of trouble is a patch that fails to apply.

Local review is out of scope. `sashiko review` already reviews the checkout it
runs in, for every project.

## Design

### Per-project upstream

`ProjectId` gains `upstream()`, next to `mailing_lists_path()` and the other
per-project answers:

```rust
pub struct Upstream {
    /// Where `origin` points and fetches come from.
    pub url: &'static str,
    /// A faster source for the initial clone, if the project has one.
    pub seed: Seed,
}

pub enum Seed {
    None,
    /// A git bundle downloaded before the clone talks to `url`
    /// (`git clone --bundle-uri`).
    Bundle(&'static str),
    /// A mirror cloned first; `origin` is then re-pointed at `url` and
    /// fetched, so only the difference comes from the upstream.
    Mirror(&'static str),
}
```

| Project | `url` | `seed` |
|---|---|---|
| linux | `https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git` | `Bundle` of the same tree's `clone.bundle` |
| sashiko | `https://github.com/sashiko-dev/sashiko.git` | `None` |
| gcc | `git://gcc.gnu.org/git/gcc.git` | `Mirror("https://github.com/gnutools/gcc.git")` |

The GCC row lands with the GCC project itself.

### `sashiko repo sync`

A new subcommand makes the tree at the repository path ready for a daemon:

```
sashiko [--project P] repo sync [--path DIR]
```

The path is `--path`, else `[git] repository_path` from the settings the
daemon would read (including `SASHIKO__GIT__REPOSITORY_PATH`), else
`third_party/<project>`. Using the daemon's own setting is what lets a
container entrypoint run `sashiko repo sync && exec sashiko` without
repeating the path.

- **Missing or empty directory:** clone from the seed, then fetch `origin`
  so the remote's refs, `origin/HEAD` and the fetch timestamps the daemon's
  sync worker reads are in place. A seeded clone that fails falls back to a
  plain clone of `url`; the seed only ever saves time. After a `Mirror`
  clone, a failed fetch from `url` is a warning: the mirror's history is
  usable on its own.
- **Directory holding only an interrupted clone** (nothing but a `.git`
  without a single ref): removed and cloned again, so a container killed
  during its first clone recovers on the next start. Anything else in the
  directory is never deleted.
- **Existing repository:** fetch `origin` through `git_ops::refresh_remote`,
  the same code path the daemon's sync worker uses, forced so the fetch
  happens now. `origin` keeps its configured URL: a developer pointing
  `repository_path` at their own clone of a fork must not have it rewritten.
  A missing `origin` is added with the project's `url`, and an `origin`
  that differs from it is reported.
- **Anything else** (a non-empty directory that is not a repository, a file)
  is an error naming the path.

All git invocations go through `git_cmd` and `GIT_PROTOCOL_RESTRICTIONS`. The
command has no timeout: the first Linux clone legitimately takes a long time,
and an operator or orchestrator decides how long to wait.

### Daemon startup refuses a missing tree

`run_daemon` checks, right after `validate_for_daemon`, that the repository
path is a git repository, and refuses to start otherwise:

```
[git] repository_path "third_party/linux" is not a git repository;
run `sashiko repo sync` to clone the linux tree there
```

An `origin` whose URL is not the project's upstream is logged as a warning,
which catches a daemon configured for one project pointed at another
project's tree.

### Entry points

- `make repo [PROJECT=linux]` runs `sashiko repo sync --path
  third_party/$(PROJECT)`. The explicit path keeps a project's tree in its own
  directory even though the repository's `Settings.toml` names
  `third_party/linux`. `scripts/setup-repo.sh` goes away.
- The image entrypoint becomes project-agnostic: environment preparation
  (`safe.directory`, directories on the volume) followed by `sashiko repo
  sync` and `exec sashiko "$@"`. `repo sync` reads the project and the path
  from the same environment and settings as the daemon, so the script names
  neither. The Sashiko and GCC deployments use it and drop their ConfigMap
  scripts.

### Linux containers clone in the foreground

Today the Linux container starts the server before the kernel exists and
clones in the background. With startup refusing a missing tree, that first
boot would crash-loop, so the Linux entrypoint syncs in the foreground, as
the Sashiko and GCC ones already do:

- A restart on an existing volume only fetches, which takes seconds.
- The first boot on an empty volume clones from the bundle and needs a
  `startupProbe` long enough to cover it, as the GCC deployment already has.
- The smoke-test request the entrypoint sends after startup stays, but waits
  for the server's `/health` instead of the `.ready` marker, which no longer
  exists.
- A platform without a persistent volume clones on every start. The image is
  deployed on GKE with a volume holding the tree, so this only matters to
  someone running the image elsewhere, who would point `repository_path` at
  a mounted clone.

## Alternatives Considered

- **One shared shell script** called by `make`, the entrypoint and the
  ConfigMaps. No Rust changes, but per-project facts stay in shell, it is
  untested, and `cargo install` users do not get it.
- **The daemon clones in-process** when the tree is missing. No setup step at
  all, but it puts a long network job inside server startup and complicates
  health checks and failure reporting. `repo sync` keeps that in a command
  that runs to completion or fails loudly.
- **Submodules per project.** Rejected in the change that removed the Linux
  submodule: every clone of Sashiko would download every project's tree.

## Rollout

1. Per-project `upstream()`.
2. `sashiko repo sync`.
3. Daemon startup refuses a missing tree.
4. `make repo` and the documentation use `repo sync`.
5. The image entrypoint uses `repo sync`; the Linux deployment's
   `startupProbe` covers a first clone.
6. The Sashiko deployment uses the image entrypoint and drops its ConfigMap
   script.
7. The GCC project adds its `upstream()` row and does the same.
