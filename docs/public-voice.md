# Public Voice

The daemon refuses to publish any bot comment, PR title,
or PR body containing strings you (the operator) or we
(the maintainers) have decided are off-limits for
public-facing text. This is the only moralizing the
codebase does. We will defend it.

## The Rule

Every outbound GitHub comment, pull-request title, and
pull-request body is checked before its corresponding API
mutation. If any of the configured forbidden strings
matches the text by case-insensitive Unicode substring,
the mutation is **refused**. The daemon does not post a
sanitised version of the text; it surfaces the violation
to the daemon log, the worker transcript, and the
operator-facing diagnostic.

This is a hard rule. There is no
`--i-know-what-im-doing` flag. There is no allowlist for
individual runs.

## Why

A real-world failure mode: an operator's automated bot
posted a comment on a public issue that mentioned the
operator's automation tooling by name. The maintainers of
the upstream project saw the bot's posts, started
poking at the names, and either:

1. Figured out the operator's internal tooling and used
   it as an attack surface, or
2. Assumed the operator was shilling for one of the named
   tools and complained.

Either way, the operator's automation was supposed to be
anonymous from the public's perspective. The public-voice
rule is what enforces that anonymity.

## The Default Forbidden Strings

The daemon ships with `comment_forbidden_strings: []` —
**the rule is implemented but the curated list is not
shipped.** Operators build their own list. A reasonable
starting list for a project that wants to keep its
internal tool names out of public comments:

- `caduceus` — The daemon's name; the bot would otherwise scream its own name
  from the rooftops.
- `opencode` — The harness the reference bridge calls.
- `gentle-ai` — The agent the reference bridge uses.
- `engram` — The canonical planning-tree name.
- `hermes` — The Hermes plugin host.
- `worker-bridge` — The bridge script's name.

These are matched case-insensitive as substrings. That
means `"Caduceus"`, `"CADUCEUS"`, and `"caduceus-bot"` all
match.

> The v0.1 README mentions a default forbidden-strings
> list, but the implementation in this repository does
> not actually ship one. The list above is a recommendation,
> not a default. Operators who want this filtering should
> set `comment_forbidden_strings` explicitly in their
> config.

## Why Substring Matching

The cheap version: substring matching catches every case
variant and every obvious evasion without writing a
parser. The cost is occasional false positives — the
string `"opencode"` matches inside `"opencodex"`. If
your comment genuinely needs to mention such a string,
you override the list.

The explicit version: substring matching is the only
matching mode that respects the spirit of the rule, which
is "don't let the bot say anything that names an
internal tool." A regex match could be evaded by adding
a Unicode confusable; a word-boundary match could be
evaded by camel-casing. Substring is the cheapest
defence that holds up against the most common evasions.

## How to Override

Set `comment_forbidden_strings` in your config:

```yaml
caduceus:
  comment_forbidden_strings:
    - "caduceus"
    - "opencode"
    - "gentle-ai"
```

**Explicit values replace the defaults entirely.** This
is on purpose. An operator who lists one forbidden string
is signalling they've thought about the rule and want
that exact list. Merging would mean an operator can't
*remove* a default they don't care about.

If you want the recommended list above plus one extra
string, copy it and add yours:

```yaml
caduceus:
  comment_forbidden_strings:
    - "caduceus"
    - "opencode"
    - "gentle-ai"
    - "engram"
    - "hermes"
    - "worker-bridge"
    - "my-org-internal-name"
```

## What Happens When a Comment Fails the Check

When the daemon refuses a comment, PR title, or PR body:

1. The worker transcript logs the violation with the
   text that failed.
2. The daemon returns the issue to `Queued` for retry.
3. The `last_error` field on the queue entry records
   the public-voice violation.
4. The worker's exit code does not matter — the daemon
   treats this as a daemon-side refusal, not a worker
   failure. **The retry budget is not consumed.** The
   issue will retry normally.

This is intentional. A harness that hardcodes the
daemon's internal name into its summary text is making
a mistake that should be fixed in the harness, not
punished by shelving the issue.

## What the Rule Does **Not** Cover

- The worker's own log files (transcripts under
  `<state_dir>/runs/`). Those are operator-private; the
  daemon doesn't post them anywhere.
- The worker's environment variables (`CADUCEUS_*`).
  The worker has them; they never leave the daemon's
  host.
- PR branch names. The daemon owns the branch name; it
  includes a daemon-internal run ID but never a
  public-voice-sensitive string.
- Commit messages authored by humans on the same
  repository. Caduceus's rule applies to commits the
  *daemon* creates; commits you make by hand are
  unchanged.

## Audit Log

Every public-voice refusal is logged to
`<state_dir>/processor.log` with:

- The issue key.
- The text that failed (the full text, including the
  forbidden substring).
- Which forbidden string matched.
- The tick ID.
- The next retry time.

Operators reviewing their daemon log can grep for
`public_voice_refusal` to find every refusal the daemon
has issued. This is intentional: silent refusal would
look like a transient infrastructure bug to the
operator; logged refusal is "your harness is naming
internal tools, fix it."
