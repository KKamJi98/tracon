# tracon

Approach control for your agent sessions. See who is holding, and go to them.

tracon is a terminal dashboard for the AI coding agent sessions running on your
own machine. It finds every live `claude` and `codex` session, shows which ones
are waiting on you and which are still working, how much of each context window
is gone, and takes you to the session you pick. Think htop, but the processes
are agents and the thing you are scanning for is which one needs you.

It reads what is already on disk and in the process table - no daemon, no
account, nothing leaves your machine.

## The name

tracon is named after TRACON, Terminal Radar Approach Control: the air traffic
facility that owns the airspace around an airport, between en-route control and
the tower. A TRACON controller does three things at once - watches every
aircraft in the area on a single scope, decides the order they get handled, and
hands each one off to whoever takes it next.

That is the job here. Every agent session on your machine shows up on one
screen, the ones that need you sort to the top, and enter hands you off to the
session itself. The tagline borrows the vocabulary too: an aircraft told to wait
for clearance is *holding*, which is exactly what a session sitting on an
approval prompt is doing.

It is pronounced TRAY-con.

## What it does

You run several `claude` and `codex` sessions across different terminals and
worktrees. Some are still working, some are stuck on an approval prompt, some
finished and are waiting for your next message. tracon watches all of them at
once and puts the ones that need you at the top of the list, in red.

```
┌tracon────────────────────────────────────────────────────────────────────────────────────────────────┐
│Waiting 2  Running 1  Idle 1  Stale 0   ctx over 85%: 1   hooks on  cmux linked                       │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌sessions──────────────────────────────────────────────────────────────────────────────────────────────┐
│AGENT  STATE    LAST   DUR    CTX%   CPU%  MODEL            PROJECT         NAME                      │
│claude waiting  3s     41m00s  18%   0.0   claude-sonnet-5  kestrel-web     checkout flake            │
│claude approval 0s     14m00s  62%   0.0   claude-opus-5    harbor-api      rate limit rollout        │
│codex  tool     1s     5m00s   44%   38.2  gpt-6-astra      meridian-cli    -                         │
│claude idle     22m00s 2h      88% ! 0.0   claude-opus-5    driftwood-infra vpc peering audit         │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
enter/r copy resume   j/k move   q quit
```

(That block is a real frame from tracon's own renderer - `cargo test --
--ignored --nocapture readme_frame` reprints it - rendered from sample
sessions. The PROJECT and NAME values are invented, not repositories or
sessions on anyone's disk.
Note the ordering: waiting rows sort above the running one, and within the same
colour the session kept waiting longest comes first. The `!` marks a session
over 85% of its context window, and the overview line counts those.)

`STATE` values: `approval` waiting for approval, `waiting` waiting for input,
`thinking` running inference, `tool` running a tool, `idle`, `stale` (untouched
for a day), `dead`, `unknown`. Select a row and press enter to copy that
session's `--resume` command to the clipboard. `ctrl-c` quits, same as `q` -
raw mode delivers it as a key, not a signal, so tracon has to handle it itself.

Two kinds of session are folded out of the list by default. Ones that have been
quiet for over a day are still running, but they are not what you opened tracon
to find. Ones driven by the SDK rather than by a person at a terminal - the
security-review subagents, for instance - cannot be waiting for you, because
nobody is sitting in front of them; tracon tells them apart by the `entrypoint`
the transcript records, and folds anything that is not `cli`. A session whose
entrypoint tracon has not read yet is left in the list: unknown means human
here. The footer says how many are folded and `a` toggles them back in. The
overview counter above keeps counting them either way, so folding hides the
rows, not the fact.

A finished turn reads as `waiting` for five minutes and then decays to `idle`.
The red is there to say *this session is asking for you*; a session you finished
reading twenty minutes ago and walked away from is not that, and leaving it red
wears the colour out.

`AGENT` says which agent the session belongs to - `claude` or `codex`. The
MODEL column usually implies it, but not always: a session whose model tracon
has not read yet shows `-` there, and that is exactly when you need to know.

`NAME` is the session's own name - the same one `claude --resume` lists. It is
the title claude generated for the session, or the one you set yourself, which
wins over the generated one. That name is what tells two sessions in the same
PROJECT apart. tracon reads it out of the transcript and never invents one from
the conversation, so a session with no name yet shows `-`, and so does every
codex session, because codex does not record one.

## Works anywhere

tracon works in any terminal - iTerm2, Ghostty, Terminal.app, a plain ssh
session. Process inspection and transcript tailing need nothing extra.

tracon never tries to move you to a session itself. Switching panes is a
different command in every terminal and multiplexer, and none of them is
present everywhere. Enter puts `claude --resume <uuid>` (or `codex --resume`)
on your clipboard instead, and you decide where to paste it. If no clipboard
helper is available - `pbcopy`, `wl-copy`, `xclip` - the command is printed in
the footer so you can still read it off the screen.

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
  as layer 1, for both claude and codex. If cmux is not installed, or its
  process dies mid-session, tracon falls back to layers 0/1 as if cmux never
  existed.

Layers stack: a session with no hooks and no cmux still shows up, just with a
lower-confidence guess. Fact-confidence sources always win over inference.

| | Layer 0: process + transcript | Layer 1: tracon hooks | Layer 2: cmux |
|---|---|---|---|
| **claude** | all states, inferred (low/medium confidence) | all states, fact (hook events) | all states, fact |
| **codex** | all states, inferred; waiting-for-input is fact when the trailing event is `task_complete` | not wired up yet - `tracon hooks install` only installs claude hooks | all states, fact, when cmux forwards codex hook events |

## Install

```sh
cargo install --git https://github.com/KKamJi98/tracon
```

A crates.io release (`cargo install tracon`) is planned but not published yet,
so the git install above is the one that works today.

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

Install the binary before installing the hooks. `tracon hooks install` writes
the absolute path of the running executable into the settings file, so a path
under `target/` (what you get from `cargo run`) stops working the moment you
run `cargo clean`. tracon refuses to install from such a path. For the same
reason, run `tracon hooks uninstall` before you delete or move the binary -
otherwise every hook event in every claude session tries to run a program that
is no longer there.

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
- Dim red does not mean "no hooks". Only one verdict renders dim red: the
  layer-0 guess that an unmatched tool call plus idle CPU means an approval
  prompt. Every other waiting verdict renders solid red, including the common
  layer-0 one (the transcript ends with an assistant message and nothing is
  pending), so a guessed waiting-for-input row looks the same as a
  hook-confirmed one. The `hooks off` and `cmux unavailable` flags in the
  overview line are what tell you how much of the screen is inferred.

## Status

Early development.
