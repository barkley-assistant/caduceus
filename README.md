# Caduceus

> A Hermes plugin that turns a labeled GitHub issue into a pull
> request, without making you babysit it.

Caduceus is a Unix daemon, shipped as a Hermes plugin, that polls
GitHub for labeled issues, runs your AI harness against them in
isolated worktrees, enforces hard timeouts, and finalizes the
result as branch → push → PR → close. Linux is tier-1; macOS
works because the supervisor is portable. Windows is not a
target. This is not the project for you if that's a problem.

We're opinionated about three things, and the rest of this
document will tell you what they are, why, and how to push
back when we're wrong:

1. **Deterministic infrastructure does not live inside the
   non-deterministic loop.** The daemon owns polling,
   claims, worktrees, timeouts, Git, GitHub, retries, and
   the public-voice rule. The worker owns "what does the
   code say, and what should it say next?" They meet at a
   single env-var contract and a single `worker-result.json`
   file. We will not put an LLM call inside our state
   machine, and we will not put a GitHub API client
   inside your harness.
2. **Zero inbound networking, no shortcuts around the
   public-voice rule.** The daemon is pull-only, refuses
   to listen on any port, and refuses to publish a
   comment or PR body containing a hardcoded list of
   internal tool names. This is the only moralizing we
   do in the codebase, and we will defend it.
3. **The bridge is a file you own.** Setup seeds a
   reference bridge at
   `~/.hermes/caduceus/worker-bridge.py`. You edit that
   file. You point it at pi, codex, claude-code, or your
   own custom harness — Caduceus has no opinion about
   which one. Plugin source updates will not overwrite
   your bridge. If the upstream bridge template changes,
   setup writes a sibling `.new` candidate and tells
   you, instead of clobbering your edits.

If you want a managed hosted product with a web dashboard
and a monthly invoice, this is not it. If you want a
single Rust binary and a Python script and the ability to
read every line of the code that runs on your behalf,
welcome.

⋆˙⟡ (ㅅ´ ˘ `) ⟡˙⋆

**A note on what this project is for**: Caduceus exists
to reduce the operator's workload, not to remove the
operator from the loop. Every PR Caduceus opens is
opened for a human to read and merge. The daemon
surfaces state and failures; humans decide what to do
about them. We are not building toward a system where a
bot ships code unattended while the maintainers sleep.
If that is what you want, this is not the project for
you either.

## How It Works

```
                ┌─────────────────────────────┐
   [GitHub]◀───▶│       Caduceus daemon       │◀─── `caduceus run`
   (outbound    │  (Rust · single binary)     │     every 2 min,
    only)       │  · ETag-aware 304 polling   │      cron-driven
                │  · whole-tick flock         │
                │  · per-issue claim files    │
                │  · isolated git worktrees   │
                │  · hard worker timeout      │
                │  · public-voice validator   │
                └──────────────┬──────────────┘
                               │  sanitized env (no gh creds)
                               │  bounded transcript pipe
                               ▼
                ┌─────────────────────────────┐
                │    your worker-bridge.py     │  ← you own this
                │   (the bridge is harness-    │     file. edit it.
                │    agnostic; ship the        │
                │    reference or your own)    │
                └─────────────────────────────┘
```

The daemon polls, picks at most one issue per tick, claims
it under a host-wide flock, provisions a worktree, spawns
the bridge as a child of a Rust worker supervisor (not
systemd, not a shell), waits for exit, then finalizes:
commit, push, find-or-create the PR, post the completion
comment, close the issue. Investigation tickets do the
same minus the commit/push/PR.

## Install (Hermes)

```bash
# Hermes Agent v0.18.2 or newer
hermes plugins install barkley-assistant/caduceus --enable
hermes caduceus setup                 # build + seed your bridge
hermes caduceus cron-install          # 2-min no-agent job
hermes caduceus status                # verify
```

The install does three things, in order, and is
idempotent:

- `cargo build --release --locked` of the Rust binary.
- Atomic install of the binary as `<plugin>/bin/caduceus`.
- Seed `~/.hermes/caduceus/worker-bridge.py` (only if
  absent; the shipped template lives in
  `plugin-assets/worker-bridge.py`).

`hermes plugins update caduceus` refreshes the source.
Rerun `hermes caduceus setup` to rebuild. Before
removal, run `hermes caduceus cron-remove` then `hermes
plugins remove caduceus`; your state, your bridge, and
your config all survive.

## Install (Standalone, No Hermes)

If you'd rather not use Hermes, you can run the binary
directly. You lose the plugin's skill, slash command,
and cron integration, but the daemon is the same:

```bash
git clone https://github.com/barkley-assistant/caduceus
cd caduceus
cargo build --release --locked
install -m 0755 target/release/caduceus ~/.local/bin/caduceus

# config at ~/.config/caduceus/config.yaml under `caduceus:`
# see docs/configuration.md for the full schema
```

A standalone install **requires** you set `worker_command`
explicitly. The daemon will refuse to start without it.
This is on purpose: the Hermes plugin has a default
bridge path; you don't, so the daemon makes you say it
out loud.

## The 60-Second Orientation

1. `git clone`, `cargo build`, `hermes caduceus setup`
   (or the standalone equivalent above).
2. Put your watched repos at `~/projects/<owner>/<repo>`
   with non-interactive Git credentials (SSH key or
   credential helper).
3. Create the two labels in each repo:

   ```bash
   gh label create "🤖 auto-fix" --repo OWNER/REPO --color 7C3AED \
     --description "Triggers Caduceus code automation"
   gh label create "🤖 auto-fix-investigate" --repo OWNER/REPO \
     --color 7C3AED --description "Triggers Caduceus investigation summary"
   ```

4. Drop the label on an issue. Wait two minutes. Watch
   `caduceus status`. When the daemon picks it up, the
   bridge runs and you get a PR.
5. **First time, run with `CADUCEUS_DRY_RUN=1`.** Dry-run
   does everything except commit / push / comment /
   label-mutate / PR / close. It writes a
   `<run_id>.dry-run.md` report under
   `<state_dir>/runs/`. You should be reading that
   report before the first real run. Trust, but verify.

## The Operator's Manual

Moved out of the README on purpose. The README is the
front door; the manual is in `docs/`:

- [`docs/installation.md`](docs/installation.md) —
  Hermes vs standalone, prerequisites, Hermes plugin
  lifecycle (install / update / remove), the cron
  contract, and the supported-host tier table.
- [`docs/configuration.md`](docs/configuration.md) —
  every config field, defaults, resolution order,
  environment variables.
- [`docs/the-bridge.md`](docs/the-bridge.md) — the
  `worker-bridge.py` contract, the `CADUCEUS_*` env
  vars, the `worker-result.json` schema, how to plug
  in a different harness.
- [`docs/state-recovery.md`](docs/state-recovery.md) —
  corrupt state, stuck issues, the `migrate-state`
  command, backup retention, cutover from v0.1.
- [`docs/public-voice.md`](docs/public-voice.md) — the
  forbidden-strings rule, why it exists, how to
  override, what happens when a comment fails the
  check.
- [`docs/architecture.md`](docs/architecture.md) —
  internal design, the worker supervisor, the lock
  discipline, why we shell out to Git instead of using
  libgit2.
- [`docs/plugin-lifecycle.md`](docs/plugin-lifecycle.md) —
  what Hermes v0.18.2 actually loads, what the
  manifest fields do, what we don't put in the manifest
  and why.
- [`docs/hermes-integration.md`](docs/hermes-integration.md) —
  cron delivery, the gateway dependency, the chat
  status surface.
- [`docs/troubleshooting.md`](docs/troubleshooting.md) —
  the common failure modes with the actual error text
  and the actual fix.
- [`docs/faq.md`](docs/faq.md) — short.

For the migration procedure from the legacy v0 cron
processor or from v0.1 itself, see
[`MIGRATION.md`](MIGRATION.md) at the repository root —
it's the operator runbook, and we keep it in the root
so a panicking operator with an outage can find it
without navigating a docs tree.

## What Caduceus Explicitly Is Not

Read this before you install it. We mean it.

- **Not a multi-host system.** Caduceus is one daemon
  per host. If you run two daemons on two machines,
  they will both poll the same org and step on each
  other. The result is not "two workers in parallel";
  it is "two workers racing for the same issue, one of
  them loses, the issue gets retried twice." Multi-host
  state with proper leader election is a post-v0.1
  conversation, and we are not going to ship a
  half-baked version of it because you asked nicely.
- **Not a GitHub App.** Caduceus uses a fine-grained
  PAT. GitHub App authentication with installation
  tokens is a future feature. We know ops teams have
  asked, we know the rotation story is better with App
  auth, we are not shipping it now because the
  migration story for v0.1 operators on PAT is more
  important than the migration story for hypothetical
  future operators on App auth.
- **Not a managed hosted service.** We don't run your
  automation. You do. There is no web dashboard, no
  monthly invoice, no Slack integration that pings us.
  The binary is yours, the daemon logs to your disk,
  and your credentials never leave your machine. If
  you want a hosted alternative, several exist; we are
  not them.
- **Not "OpenCode inside the daemon".** The daemon has
  absolutely no opinion about which LLM you call. We
  ship a reference bridge because every project needs
  a starting point; the bridge currently calls
  OpenCode because that's what we use internally. Swap
  the bridge for pi, codex, claude-code, or your own
  script, and the daemon will not notice or care. See
  `docs/the-bridge.md` for the contract.
- **Not a replacement for code review.** Every PR that
  Caduceus opens is opened for a human to review and
  merge. There is no auto-merge today. Policy-gated
  auto-merge with a documented policy in plain English
  is a future feature, not a current one.
- **Not a webhook receiver.** The daemon is pull-only.
  It polls GitHub on a schedule. We will never accept
  inbound HTTP. If you want push semantics, write a
  webhook → label-relabel shim in front of Caduceus;
  that's your shim, not ours.
- **Not a queue you can attach a custom worker to.**
  The worker contract is `worker-bridge.py` plus the
  `CADUCEUS_*` env vars plus the `worker-result.json`
  file. That's it. If you want to bypass that
  contract, you don't want Caduceus; you want a job
  queue.

## Contributing, Releasing, SemVer

This project follows [Semantic Versioning 2.0.0](https://semver.org/).
The public surface — `caduceus` CLI, the `Config` YAML
schema, the plugin manifest fields, the
`worker-bridge.py` env-var contract, the state file
format, the default `comment_forbidden_strings` — is
versioned; everything else is implementation detail and
can change between minor releases.

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to file
  issues, open PRs, what the CI expects, the commit
  format we use.
- [`RELEASING.md`](RELEASING.md) — SemVer policy, what
  counts as a breaking change, the release cadence
  (or lack of one), how release tags are cut and what
  CI runs on them.
- [`CHANGELOG.md`](CHANGELOG.md) — keep-a-changelog
  format. Every user-visible change lands an entry.
- [`AGENTS.md`](AGENTS.md) — agent guidance for both
  human contributors and AI tools. Read it before
  opening a PR; the constraints on state files, the
  contract-revision procedure, the test discipline,
  and the no-edits-to-published-prompts rule live
  there.

## License

MIT. See [`LICENSE`](LICENSE).
