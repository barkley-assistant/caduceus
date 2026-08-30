# caduceus-worker-reference image

The minimal deterministic reference worker image for the Caduceus OCI
worker contract. It proves the contract end to end — canonical
`CADUCEUS_*` environment reading, `/workspace` access, schema-valid
`/output/worker-result.json` writing, and the certification probes —
and is used as a test fixture, a CI artifact, and an operator example.
It is **never** a production dependency of the executor: the production
code in `src/` does not reference the image (an independence test
enforces this), and a future Doctor diagnostic may optionally run it as
an isolated canary.

## What the image contains

Everything is copied at fixed paths into a `busybox:1.36.1` base that
is pinned by its linux/amd64 SHA256 manifest digest, so the build never
floats on a mutable tag. There is no package manager, compiler,
language runtime, or LLM tooling — busybox POSIX `sh` and coreutils are
the entire runtime.

| Path | Purpose |
|---|---|
| `/usr/local/bin/caduceus-env.sh` | Contract helper (WR-2) |
| `/usr/local/bin/write-result.sh` | Result writer (WR-3) |
| `/usr/local/bin/worker-probe` | Probe subcommand dispatcher (WR-4) |
| `/usr/local/bin/probes/sentinel-read.sh` | Reads a sentinel from `/workspace` |
| `/usr/local/bin/probes/mount-probe.sh` | Verifies `/workspace`, `/output`, `/tmp` |
| `/usr/local/bin/probes/resource-hog.sh` | Bounded CPU + memory allocation |
| `/usr/local/bin/probes/network-probe.sh` | Reachability per network mode |

The default command prints a one-line contract summary. Nothing relies
on it: the executor passes arbitrary `--entrypoint` argv, and every
script is reachable at its fixed path.

## The contract it proves

- **WR-1 — deterministic, minimal contents.** Digest-pinned base;
  only the helper, writer, probes, and busybox utilities.
- **WR-2 — canonical environment.** `caduceus-env.sh` reads the eleven
  canonical `CADUCEUS_*` variables (mirroring
  `src/executor/sandbox_spec.rs::CANONICAL_ENV_KEYS`). Without options
  it prints `NAME=VALUE` per line — multi-line values (e.g.
  `CADUCEUS_ISSUE_BODY` in real worker runs) are collapsed to spaces so
  every variable stays on exactly one line; `--names-only` prints the
  sorted names; any unset variable exits non-zero and names it.
- **WR-3 — schema-valid result.** `write-result.sh` honors
  `CADUCEUS_RESULT_PATH` (default `/output/worker-result.json`),
  writes `status`/`summary`/`commit_message`/`pull_request_title`, uses
  a temp file + atomic rename so no partial file survives, and exits
  non-zero with a diagnostic when the target is not writable.
  `--status failure` writes a schema-valid failure document.
- **WR-4 — certification probes.** Each probe prints a single-line
  `PASS` report and exits zero on success, or a diagnostic and non-zero
  on failure:

  - `worker-probe sentinel-read [path]` — reports the contents of
    `/workspace/sentinel.txt` (or the given path).
  - `worker-probe mount-probe` — verifies `/workspace` and `/output`
    are writable and `/tmp` is a writable bounded tmpfs.
  - `worker-probe resource-hog [cpu_seconds] [memory_kib]` — allocates
    within hard bounds (10 s CPU, 32 MiB memory).
  - `worker-probe network-probe [none|unrestricted|auto]` — requires
    no reachability for `none`, requires reachability for
    `unrestricted`, and reports either way for `auto` (default).

## Build

```sh
docker build --pull \
  -f plugin-assets/worker-reference-image/Containerfile \
  -t caduceus-worker-reference:local \
  plugin-assets/worker-reference-image
```

CI builds the same image locally and runs the contract smoke in the
`oci-reference-image` job of `.github/workflows/ci.yml`; it never
pushes. Publication happens only from the release workflow
(`.github/workflows/release-worker-image.yml`) on `v*` tags, which
pushes `ghcr.io/barkley-assistant/caduceus-worker-reference:vX.Y.Z`
and `latest` with provenance and echoes the published digest into the
workflow summary and release notes.

## Operator example

Run the image like the executor does: read-only rootfs, `/workspace`
and `/output` bind mounts, bounded `/tmp` tmpfs, and arbitrary
`--entrypoint` argv with the canonical environment:

```sh
ws="$(mktemp -d)" && out="$(mktemp -d)"
echo "example-sentinel" > "$ws/sentinel.txt"

docker run --rm --read-only \
  --network none --tmpfs /tmp:size=256m \
  -v "$ws":/workspace:rw -v "$out":/output:rw \
  -e CADUCEUS_RUN_ID=example \
  -e CADUCEUS_ISSUE_ID=example \
  -e CADUCEUS_ISSUE_NUMBER=1 \
  -e CADUCEUS_ISSUE_REPO=owner/repo \
  -e CADUCEUS_ISSUE_TITLE="Example run" \
  -e CADUCEUS_ISSUE_BODY="Example body" \
  -e 'CADUCEUS_ISSUE_LABELS_JSON=["example"]' \
  -e 'CADUCEUS_CONTEXT_JSON={}' \
  -e CADUCEUS_BRANCH_NAME=main \
  -e CADUCEUS_WORKTREE_PATH=/workspace \
  -e CADUCEUS_RESULT_PATH=/output/worker-result.json \
  --entrypoint /bin/sh \
  caduceus-worker-reference:local -c '
    /usr/local/bin/caduceus-env.sh --names-only &&
    /usr/local/bin/worker-probe sentinel-read &&
    /usr/local/bin/worker-probe mount-probe &&
    /usr/local/bin/worker-probe resource-hog 1 1024 &&
    /usr/local/bin/worker-probe network-probe none &&
    /usr/local/bin/write-result.sh &&
    cat /output/worker-result.json
  '
```

## Fixture separation

OCI tests that need a real container must use the unrelated fixture
image (`tests/fixtures/oci-fixture-image/`), not this reference image.
The fixture is Debian-slim based so it shares no layers with this
busybox image, and the fixture-parity test
(`tests/architecture/oci_fixture_parity_test.rs`) enforces the
separation.

## Canary isolation

The image is stateless and self-contained: it reads only the canonical
environment and the sandboxed `/workspace`, `/output`, `/tmp` surfaces
and writes only the result document. A future Doctor diagnostic may
optionally run it as an isolated canary subject without creating an
executor dependency — the independence test
(`tests/architecture/oci_independence_test.rs`) keeps `src/` free of
any reference to the image.