# Human review checkpoint

This artifact is required only for a manifest task that declares `human_review.required`. It must be completed by a human reviewer, not by the autonomous implementation loop.

- Task ID:
- Reviewer identity:
- Review date/time:
- Reviewed commit or workspace revision:
- Tests observed or independently run:
- Process/lifecycle paths inspected:
- Findings and required follow-up:
- Approval: approved | changes requested

For Task 5.1, the reviewer must explicitly address daemon death/control-pipe EOF, direct-child exit with live descendants, detached-session descendants on Linux, TERM-to-KILL escalation, child reaping, transcript draining, heartbeat removal, and inherited descriptor handling.
