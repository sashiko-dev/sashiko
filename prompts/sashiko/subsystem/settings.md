# Settings

`src/settings.rs` defines the entire configuration surface. It guarantees one
thing above all: **a configuration file is either fully understood or the
process refuses to start.** Every struct in the file except two carries
`#[serde(deny_unknown_fields)]`, so there is no such thing as a tolerated
stray key, a renamed key with a grace period, or a removed key that old files
may keep. That guarantee is what makes the compatibility rules below hard
rules rather than advice.

Files in scope: `src/settings.rs`, `Settings.toml` (the daemon's own shipped
configuration, and the one `test_production_settings_is_valid` parses),
`docs/examples/Settings.example.toml` (embedded into the binary as
`DEFAULT_SETTINGS` in `src/main.rs` and written verbatim by `sashiko init`),
`docs/configuration.md` (the reference table), and the deployment manifests
that set `SASHIKO__*` (`Dockerfile`,
`deployment/sashiko.dev/base/app/sashiko-k8s.yaml`,
`scripts/docker-entrypoint.sh`).

## 1. The load path

```
Settings::new()            -> Settings::from_file("Settings")
Settings::from_file(path)  -> config::File::from(path)
                            + config::Environment::with_prefix("SASHIKO").separator("__")
                            -> try_deserialize::<Settings>()
```

- `Settings::new()` passes the *stem* `"Settings"`, so the `config` crate
  resolves `Settings.toml` relative to the current working directory. A
  missing file is a hard `ConfigError`; the source is required, not optional.
- Environment overrides win over the file, because the `Environment` source is
  added second.
- `separator("__")` is used by config-rs **both** between the prefix and the
  first key and between nested keys. So it is `SASHIKO__AI__PROVIDER`, never
  `SASHIKO_AI__PROVIDER`. Commit `21b1a1807d12` ("env var examples in README
  and code comment use wrong separator") exists because the docs and the
  in-code comment got this wrong while production already used the right form.
  If a diff adds an env example anywhere, check the separator.
- The environment is *always* consulted, including by every test that calls
  `Settings::new()`. A `SASHIKO__*` variable exported in a shell can make tests
  pass or fail.

### The environment cannot express a TOML array

config-rs hands every environment value to serde as a string. A `Vec<String>`
field set from the environment therefore fails with "invalid type: string,
expected a sequence" — which is a *startup failure*, not a lost setting.

Only the fields that opt in via `deserialize_with = "deserialize_string_or_vec"`
accept a comma-separated string:

- `MailingListsSettings::track` (added by `8b04c9b0616b`)
- every list on `AclSettings`: `admins`, `security`, `bug_reporters`,
  `ingest`, `cancel`, `review`, `blocklist` (added by `0516844f4f0d`, whose
  message records that `SASHIKO__SERVER__ACL__ADMINS` previously refused to
  start the process)

These do **not** accept it, and so cannot be set from the environment at all:
`ReviewSettings::ignore_files`, `GitSettings::custom_remotes`,
`SubsystemsSettings::mapping`, `CustomRemoteSettings::only_branches`.

**When a diff adds a `Vec` field**, ask whether a deployment will want to set
it from the environment. If yes it needs `deserialize_string_or_vec`; if no,
say so in the doc comment. `deserialize_string_or_vec` drops empty entries, so
an empty variable yields an empty list rather than a list with one blank
member — `test_acl_lists_read_a_string_as_well_as_an_array` pins that, and it
is what keeps `AclSettings` fail-closed against a caller presenting no address.

## 2. `deny_unknown_fields` is on everything except two structs

Carrying it: `SubsystemMapping`, `SubsystemsSettings`, `ProjectSettings`,
`ForgeSettings`, `DatabaseSettings`, `NntpSettings`, `SmtpSettings`,
`MailingListsSettings`, `ClaudeSettings`, `GeminiSettings`, `BedrockSettings`,
`VertexSettings`, `OpenAiCompatSettings`, `VllmSettings`, `OllamaSettings`,
`KiroCliSettings`, `GooseCliSettings`, `ClaudeCliSettings`, `CodexCliSettings`,
`DevinCliSettings`, `AiSettings`, `AclSettings`, `ServerSettings`,
`CustomRemoteSettings`, `GitSettings`, `ReviewSettings`, `Settings`.

Not carrying it, deliberately: `LocalReviewSettings` and
`LocalReviewReviewSettings` (§4).

Consequences a reviewer must apply to any diff that changes a field name:

1. **Removing or renaming a key breaks every existing configuration file at
   startup.** Not "falls back to the default" — `try_deserialize` returns
   `ConfigError` and `main()` returns `Err`. Commit `ef7b8ce64277` removed
   `[linux_bug]` and `[server.sign_in_link]`; every deployment carrying those
   sections had to drop them in the same change. A diff that deletes a field
   from a struct must delete it from `Settings.toml`,
   `docs/examples/Settings.example.toml`, `docs/configuration.md`, and the k8s
   manifest, or a deployment that already has it stops booting.
2. **Adding a key to a TOML file without adding it to the struct also breaks
   startup.** A documentation-only change that shows a new key in
   `docs/configuration.md` is not harmless: someone will copy it.
   `docs/configuration.md` currently documents `[linux_bug]`, which no longer
   exists in `Settings` — copying that section into a real file makes the
   daemon refuse to start. Treat a diff that adds a doc row without a matching
   field the same way.
3. **`#[serde(skip)]` fields are unknown fields.** `ReviewSettings::stages` and
   `AiSettings::no_ai` are skipped, so writing `stages = [...]` or
   `no_ai = true` in a TOML file is an error, not a no-op. They are set from
   the command line (`--stages`, `--no-ai`) in `src/main.rs`. A diff that adds
   a CLI-only knob must use `skip` *and* must not document it as a TOML key.
4. **Feature-gated sections are unknown fields on a default build.**
   `AiSettings::bedrock` is behind `#[cfg(feature = "bedrock")]` and
   `AiSettings::vertex` behind `#[cfg(feature = "vertex")]`; neither is in
   `default` in `Cargo.toml`. A `Settings.toml` containing `[ai.bedrock]`
   parses on `--features bedrock` and fails everywhere else. This is why both
   sections are commented out in the shipped `Settings.toml`. A diff that
   uncomments one, or that adds a new feature-gated provider section to an
   example file, breaks the default build.
5. **Typos are the point.** `test_acl_rejects_unknown_keys` asserts that
   `securty = [...]` fails, because the ACL lists are the whole authorization
   model and a silently ignored misspelling would grant nothing while looking
   like it granted something. Do not accept a diff that relaxes
   `deny_unknown_fields` on a security-bearing struct to "ease migration".

## 3. Required versus optional

`Settings` itself:

| Field | Shape | Effect of omitting the section |
|---|---|---|
| `log_level` | `default = "info"` | fine |
| `project` | `#[serde(default)]` → `ProjectSettings::default()` | fine |
| `subsystems` | `default_subsystems()` | fine |
| `forge` | `default_forge()` (disabled, `disable_nntp = true`) | fine |
| `database` | **required** | startup error |
| `nntp` | **required** | startup error |
| `smtp` | `Option<SmtpSettings>` | mail disabled |
| `mailing_lists` | **required** | startup error |
| `ai` | **required** | startup error |
| `server` | **required** | startup error |
| `git` | **required** | startup error |
| `review` | **required** | startup error |

Within a required section, a field with no `#[serde(default)]` is itself
required: `database.url`, `database.token`, `nntp.server`, `nntp.port`,
`ai.provider`, `ai.model`, `server.host`, `server.port`,
`git.repository_path`, `review.concurrency`, `review.worktree_dir`,
`mailing_lists.track`, and, inside `[smtp]` when present, `sender_address`.
`smtp.server` and `smtp.port` are `Option` because the `sendmail` transport
has no use for them; `SmtpSettings::validate()` requires both when
`transport` is `smtp` (the default) and rejects credentials when it is
`sendmail`.

**Adding a field to a required section without `#[serde(default = ...)]` or
`Option` breaks every existing configuration file.** That is a decision, not
an accident: `f0c1cff04ce3` deliberately made `review.concurrency` required
for local reviews too, on the grounds that "how many patches to review at once
depends on the machine, so a wrong guess is worth an error rather than a quiet
default", and in the same commit added the section to
`docs/examples/Settings.example.toml` and to `docs/sashiko-cli.md`. If a diff
makes a field required, look for the matching updates to every checked-in
configuration and example.

Two defaults are deliberately safe rather than convenient, and a diff that
flips either should be challenged: `SmtpSettings::dry_run` defaults to `true`
(`default_dry_run`), and `ServerSettings::log_sign_in_links` defaults to
`false` (a sign-in link is a bearer credential; see
`test_sign_in_link_logging_is_off_unless_asked_for`).

## 4. `LocalReviewSettings` is a second, independent projection of the same file

```rust
pub struct LocalReviewSettings { pub ai: AiSettings, pub review: LocalReviewReviewSettings }
pub struct LocalReviewReviewSettings { pub concurrency: usize, pub timeout_seconds: u64 }
```

Neither has `deny_unknown_fields`. Commit `2afd5c82f37b` removed it because
these structs are "a schema projection for parsing only ai and review subset
blocks from the root Settings.toml or sashiko.toml", and keeping it made a
local review reject `log_level`, `worktree_dir` and every other real key in
the daemon's file.

Consequences:

- A local review reads `[ai]` through the *same* `AiSettings` type, which
  **does** have `deny_unknown_fields`. So an unknown key under `[ai]` still
  fails, in both paths.
- A local review reads `[review]` through a **different** type that silently
  ignores `worktree_dir`, `max_retries`, `max_total_tokens`, `ignore_files`
  and everything else. The two views of `[review]` can drift apart with no
  compile error and no test failure. **If a diff adds a `[review]` key that a
  local review must honour, it has to be added to `LocalReviewReviewSettings`
  as well.** Today only `concurrency` and `timeout_seconds` are honoured
  locally; `6907509cbd13` ("honour timeout_seconds in worker-run reviews") is
  the shape of what goes wrong when one is missed.
- A diff must never re-add `deny_unknown_fields` to either struct.

Resolution order, in `Settings::local_review_path_in`: `./Settings.toml` if it
exists, else `Settings::user_config_path()`, which is
`$XDG_CONFIG_HOME/sashiko.toml`, else `$HOME/.config/sashiko.toml`, else the
relative `.config/sashiko.toml`. `Settings::local_review_path()` is that with
base `.`.

`local_review::run_worker` picks one of three loaders, and they do not agree
on shape:

1. `options.settings_path` set → `Settings::local_review_from_file(path)`
   (projection; tolerates a partial file).
2. no settings path but a `repo_override` → `Settings::local_review_settings()`
   (projection, at `local_review_path()`).
3. neither → `Settings::new()` (**full** `Settings`; a partial
   `sashiko.toml` fails here).

A diff that changes which branch an entry point lands in changes which fields
are required of the user's file. `review_current_tree` currently forces branch
1 by defaulting `settings_path` to `Some(Settings::local_review_path())`.

`test_init_template_satisfies_local_review` pins that
`docs/examples/Settings.example.toml` parses as `LocalReviewSettings` — i.e.
that the file `sashiko init` writes is usable by `sashiko review`. Any change
to that template must keep that test passing, and any change to
`LocalReviewSettings` must keep the template valid.

## 5. `local_token_path` follows the database

`Settings::local_token_path()` derives, never configures, where the server's
local operator token lives:

- `database.url` containing `"://"` (a remote libsql URL) → `.` (the working
  directory).
- otherwise → `Path::new(url).parent()`, empty parent → `.`.
- filename is always `LOCAL_TOKEN_FILE_NAME` = `.sashiko-local-token`.

The rationale in the doc comment is the contract: the database is the one
thing the server, `sashiko-cli` and `benchmark` already agree on, so a client
that reads a different configuration than the server is by definition not
local to it. Writers and readers: `src/main.rs` (`publish_local_token`, and
the `remove_file` on shutdown), `src/bin/sashiko-cli.rs` (`build_client`),
`src/bin/benchmark.rs`.

**Changing `database.url` moves the token file.** A diff that changes the
derivation, or that adds a configurable override, breaks the three readers
above unless it changes them together. `test_local_token_path_follows_the_database`
covers the three cases; keep it.

## 6. `validate_sign_in_delivery`

Called once, in `src/main.rs`, after the CLI overrides are applied and before
the database is opened. It returns `Err` when `smtp` is `Some` and
`server.public_base_url` is either absent or not
`is_reachable_base_url(...)` — the wildcard forms `::`, `[::]`, `0.0.0.0`,
`*`, plus `localhost`, `127.0.0.1`, `::1`, a non-`http(s)` scheme, and a bare
host with no scheme are all rejected. `main()` turns that into a refusal to
start.

It is **not** called on the `Init`, `Review` or `Worker` subcommand paths:
those return from `main()` before reaching it. So a validation added here
protects the daemon only. If a diff adds a new cross-cutting validation, check
whether it belongs before the subcommand dispatch instead.

`ServerSettings::sign_in_base_url()` is the weaker sibling: it falls back to
`http://{host}:{port}` and is only good enough for a link written to a local
operator's log. Do not let a diff use it for anything mailed.

## 7. `[project].kind`

`ProjectSettings::kind: Option<ProjectId>`, added by `a0bc5c129a0a`.
`ProjectId` is `#[serde(rename_all = "lowercase")]`, so the value is `"linux"`
or `"sashiko"`.

- Absent means "any", because a file written before projects existed cannot
  name one. Do not let a diff make it required.
- `main::effective_project(cli.project, settings.project.kind)` resolves it
  and **errors** when the `--project` flag names a different project than the
  file, because a settings file points at a database, a git tree and a port.
- `src/main.rs` then writes the answer back: `settings.project.kind =
  Some(project)`, so everything downstream reads one resolved value rather
  than re-deriving it. `run_review_tool` relies on this when it passes
  `--project settings.project.kind.unwrap_or_default().as_str()` to the worker
  child — the child runs with `env_clear()` and would otherwise fall back to
  `ProjectId::default()` (Linux) and load the kernel prompts.
- `src/bin/sashiko-cli.rs` applies the same precedence for its own `PROJECT`.

A diff adding a `ProjectId` variant must also add it to `ProjectId::ALL`
(`test_all_lists_every_project` asserts the count), give it a `prompt_dir()`
that actually exists in the bundle, and decide `uses_maintainers()`. Note
`prompt_dir()` is deliberately not `as_str()` — Linux maps to `kernel` — and
`test_prompt_dir_is_not_the_project_name` exists to stop that being
"simplified".

## 8. Parse failures must stay loud

Commit `5820c093856c` made `sashiko-cli` panic when `Settings.toml` exists but
fails to parse, rather than falling back to localhost and silently talking to
the wrong server. The surviving code in `src/bin/sashiko-cli.rs` distinguishes
the two cases by matching the error *string*:

```rust
Err(e) => {
    if e.to_string().contains("not found") {
        "http://127.0.0.1:8080".to_string()
    } else {
        panic!("Failed to parse Settings.toml: {}", e);
    }
}
```

That is a brittle match on a `config` crate message, and it is the only thing
separating "no file, use defaults" from "broken file, stop". A diff that
upgrades `config`, or that wraps the error, can silently turn every parse
failure back into a localhost fallback. If you see the `config` dependency
move in `Cargo.toml`, check this site.

The same "fail loud on a malformed file" rule is enforced elsewhere for the
sibling config: `fe706adb7b23` replaced `EmailPolicyConfig::load(...)
.unwrap_or_default()` with a hard failure, because an empty default has
`mute_all = true` and silently muted every reply.

## Checklist for a diff that adds, renames or removes a setting

Adding a key:

- [ ] Field has `#[serde(default = "...")]` or is `Option`, unless breaking
      every existing file is the deliberate intent.
- [ ] The default matches the value written in `Settings.toml` and in
      `docs/examples/Settings.example.toml`, or those files are updated.
- [ ] `docs/configuration.md` gains a row with the same default.
- [ ] If it is a `Vec`, it either has `deserialize_string_or_vec` or is
      documented as file-only.
- [ ] If it lives under `[review]` and a local review needs it, it is also on
      `LocalReviewReviewSettings`.
- [ ] If it is CLI-only, it is `#[serde(skip)]` and is *not* documented as a
      TOML key.
- [ ] If it is feature-gated, the example files leave it commented out.

Renaming or removing a key:

- [ ] Every checked-in file that sets it is updated in the same commit:
      `Settings.toml`, `docs/examples/Settings.*.toml`,
      `docs/configuration.md`,
      `deployment/sashiko.dev/base/app/sashiko-k8s.yaml`,
      `Dockerfile`, `scripts/docker-entrypoint.sh`, `README.md`.
- [ ] The commit message says that deployments carrying the old key will fail
      to start. There is no compatibility shim; `deny_unknown_fields` forbids
      one.
- [ ] Any `SASHIKO__<OLD>__<NAME>` in the manifests is renamed, with the `__`
      separator in both positions.

Any change to the load path:

- [ ] `test_production_settings_is_valid` and
      `test_init_template_satisfies_local_review` still pass; they are the
      only guards that the shipped files match the structs.
- [ ] The `Environment` source is still added *after* the `File` source, so
      the environment still wins.
- [ ] `LocalReviewSettings` / `LocalReviewReviewSettings` still lack
      `deny_unknown_fields`.
