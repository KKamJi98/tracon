# tracon

Approach control for your agent sessions. See who is holding, and go to them.

tracon lists every AI coding agent session running on your machine, tells you
which ones are waiting on you, and takes you there.

Works in any terminal. iTerm2, Ghostty, Orca, Terminal.app, plain ssh.
tmux and cmux are optional adapters that add jump support.

Status: early development.

## Known limitations

- codex: a waiting-for-approval state is inferred (idle CPU plus an unmatched
  tool call), never observed. codex rollout files carry no approval-request
  record, so there is no fact-based signal for this state the way there is
  for a trailing `task_complete` (which does map to a fact-confidence
  waiting-for-input state).
