# Recorded provider observations

This is a curated record of prior maintainer runs on macOS arm64, published
on 2026-11-18. It is not a new live qualification run. The original runs used
private masil sockets, temporary working directories and authenticated local
provider installations where available. Raw conversations, account data and
machine-specific paths are not published here.

The support catalog's capability list names what was exercised, with the
limitations below. It is not a certification that every state transition or
operation passed. A matching installed version still needs run-specific
integration and authentication checks.

## claude

2026-11-17, Claude Code 2.1.326: hook-backed prompts were accepted, lifecycle
state and subagent completion were observed, and restarting with the recorded
session produced a verified resume report. Interrupt delivery initially had
an unconfirmed result; subsequent manual tests observed the interrupted turn
return to idle after about 20 seconds. The final draft checks refused a prompt
over existing composer text and delivered after it was cleared. These runs
required the exported hook. Screen-only streaming can still look idle.

## codex

2026-11-17, Codex 0.169.0: prompt delivery and interrupt with a
`turn_end_observed` result were exercised. Draft rejection and later delivery
with an empty composer were checked. Delivery did not establish native
acceptance. An older 0.160.0 manual resume showed the previous conversation,
but its missing callback left binding awaiting; that is not a verified resume
entry in the catalog.

## opencode

2026-10-26, OpenCode 1.19.4: one bash permission was read and answered on an
`--answers` run, producing `native_accepted`. This is a single request example,
not coverage of every permission and question shape.

2026-11-17, OpenCode 1.19.5: prompt acceptance and interrupt were exercised with
`--answers`. Without that channel the initial screen was unknown. Versions
1.19.5 and 1.19.6 were also inspected at widths 80 and 150 for working-screen
rules. The 1.19.6 record covers screen inspection, not a separate prompt and
interrupt qualification. These November runs do not add a new approval-answer
qualification for either version.

## kilo

2026-10-26, Kilo 7.8.17: one bash permission was read and answered through
`--answers`, producing `native_accepted`.

2026-11-17, the same version: prompt delivery, approval/rejection screens and
working indicators were observed. Later that day, two Escape presses ended a
working turn and masil reported `turn_end_observed`. Prompt delivery had no
provider acceptance digest. Screen-only prompt readiness was not qualified.

## omp

2026-11-17, OMP 18.12.2: the exported extension supplied lifecycle and session
evidence. Resume was verified. After a turn, masil delivered another prompt
after 46 seconds of idle. A visible `Working…` line allowed interrupt, ending
with `turn_end_observed`. Approval screens were inspected at 80 columns and
draft rejection was checked. Later recap output can invalidate the idle report
and return the state to unknown. Before the first lifecycle report, prompt
can be refused.

## pi

2026-10-04, Pi 0.82.1: extension lifecycle and resume reports were observed.
A prompt could not complete because OAuth refresh returned 401.

2026-11-17, Pi 0.84.2: state changed from unknown to an idle report after direct
input, but OAuth refresh again failed before model execution. This is limited
state evidence, not a successful prompt test.

## cursor

2026-11-17, Cursor Agent 2026.10.22: only the logged-out screen was inspected.
A detection rule for that screen was added. Earlier manual key tests showed
that paste and Enter must be separated, but masil's complete two-step prompt
path was not live-qualified by the November run.

## agy

2026-10-26, Antigravity CLI 1.2.16: the idle screen matched the existing rule.
This record does not qualify prompt, interrupt or resume for that version.

## kimi

2026-10-04, Kimi 2.1.1: the folder trust prompt was recognized as blocked.
Authentication rejected the stored token, so working, prompt and interrupt
were not qualified.

## gemini

2026-11-17, Gemini CLI 0.46.0: folder trust and login selection screens were
observed as blocked. Authentication failed with an unsupported-client message.
Prompt, interrupt and resume were not qualified.

## mastracode

2026-11-17, MastraCode 0.50.0: only the setup wizard was inspected; state was
unknown. No capability is listed as live-qualified from this observation.
