# tracon

Approach control for your agent sessions. See who is holding, and go to them.

tracon lists every AI coding agent session running on your machine, tells you
which ones are waiting on you, and takes you there.

## What it does

You run several `claude` and `codex` sessions across different terminals and
worktrees. Some are still working, some are stuck on an approval prompt, some
finished and are waiting for your next message. tracon watches all of them at
once and puts the ones that need you at the top of the list, in red.

```
+ tracon ------------------------------------------------------------------+
| Waiting 2  Running 1  Idle 1  Stale 0     hooks on   cmux linked         |
+----------------------------------------------------------------------------+
+ sessions -------------------------------------------------------------+
| ST  LAST   DUR    CTX% CPU%  MODEL             PROJECT       JUMP        |
| WA  0s     14m    62%  0.0   claude-opus-5     acme-api       tmux:main:1.2|
| WI  3s     41m    18%  0.0   claude-sonnet-5   nebula-web     tmux:main:2.1|
| RT  1s     5m     44%  38.2  gpt-5-codex       lighthouse-cli -           |
| ID  22m    2h      9%  0.0   claude-opus-5     forge-infra    -           |
+----------------------------------------------------------------------------+
```

(The output above is composed for illustration; PROJECT values are invented
names, not real repositories.)

`ST` codes: `WA` waiting for approval, `WI` waiting for input, `RI` running
inference, `RT` running a tool, `ID` idle, `ST` stale (untouched for a day),
`DE` dead, `??` unknown. Select a row and press enter to jump to it, or copy
its `--resume` command if there is nowhere to jump to.

## Works anywhere

tracon works in any terminal - iTerm2, Ghostty, Terminal.app, a plain ssh
session. Process inspection and transcript tailing need nothing extra.

tmux and cmux are optional adapters. They do not change what tracon can see;
they only add a jump target to the `JUMP` column, so pressing enter actually
switches you to that pane or workspace instead of just printing a
`--resume` command to copy.

## Three data layers

tracon combines up to three sources per session, and always has a usable one:

- **Layer 0 - process + transcript (always on).** tracon walks running
  `claude`/`codex` processes and tails their transcript files. This always
  works, with no setup, but it infers state from CPU and recent transcript
  entries - a session that looks idle because it is waiting on your approval
  looks the same, from the outside, as one that is idle for other reasons.
- **Layer 1 - tracon's own hooks (claude only, opt-in).** `tracon hooks
  install` wires tracon into claude's hook events (`PermissionRequest`,
  `Stop`, `Notification`, and friends). Once installed, claude's state is an
  observed fact instead of a guess - install-and-wait vs.
  running-a-tool-while-idle stop looking alike.
- **Layer 2 - cmux (optional, when present).** If cmux is running, tracon
  subscribes to its event stream. This gives the same fact-confidence states
  as layer 1, for both claude and codex, plus a `cmux:<workspace>` jump
  target. If cmux is not installed, or its process dies mid-session, tracon
  falls back to layers 0/1 as if cmux never existed.

Layers stack: a session with no hooks and no cmux still shows up, just with a
lower-confidence guess. Fact-confidence sources always win over inference.

| | Layer 0: process + transcript | Layer 1: tracon hooks | Layer 2: cmux |
|---|---|---|---|
| **claude** | all states, inferred (low/medium confidence) | all states, fact (hook events) | all states, fact + jump target |
| **codex** | all states, inferred; waiting-for-input is fact when the trailing event is `task_complete` | not wired up yet - `tracon hooks install` only installs claude hooks | all states, fact + jump target, when cmux forwards codex hook events |

## Install

```sh
cargo install tracon
```

Supported platforms: macOS and Linux. Windows is out of scope.

## Hooks

```sh
tracon hooks install    # writes the hook block into ~/.claude/settings.local.json
tracon hooks uninstall  # removes only the entries tracon added
tracon hooks print      # prints the hook JSON fragment to stdout
```

`tracon hooks install` edits `~/.claude/settings.local.json` directly: it
backs the existing file up, merges tracon's hook entries into it, and leaves
everything else in the file untouched. Use this if you manage that file by
hand.

`tracon hooks print` does not touch any file - it prints the same JSON
fragment so you can paste it into whatever generates your claude settings
(a dotfiles repo, a config-management tool, and so on). Use this if
`~/.claude/settings.local.json` is itself a generated artifact in your setup.

## `--json`

```sh
tracon --json
```

Prints one snapshot as JSON and exits, instead of opening the TUI. This is
meant for a statusline or a script that polls tracon on its own schedule
rather than watching the live view - each call is a fresh, independent
snapshot.

## Known limitations

- codex: a waiting-for-approval state is inferred (idle CPU plus an unmatched
  tool call), never observed. codex rollout files carry no approval-request
  record, so there is no fact-based signal for this state the way there is
  for a trailing `task_complete` (which does map to a fact-confidence
  waiting-for-input state).
- A waiting verdict produced by layer 0 alone (no hooks, no cmux) renders as
  dim red instead of solid red. It is tracon's best guess from process and
  transcript state, not an observed fact - solid red is reserved for
  waiting states confirmed by layer 1 or layer 2.

## Status

Early development.
