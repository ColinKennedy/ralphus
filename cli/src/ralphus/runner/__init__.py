"""The ralphus session runner.

The daemon dispatches one runner subprocess per session, handing it a
`SessionSpec` as JSON and reading back a `SessionResult` as JSON. The runner is
where a session actually *does work* — the exact step the predecessor project
got wrong (see ``../../../FINDINGS.local.md`` §2). Keeping it a small,
self-contained, unit-tested Python component (rather than shelling out to a
whole agent harness) is what makes it testable against a local model.
"""

from ralphus.runner.spec import SessionResult, SessionSpec

__all__ = ["SessionResult", "SessionSpec"]
