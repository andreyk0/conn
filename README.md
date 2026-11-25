# conn

`conn` is a single-binary local tool for one developer running many Claude Code terminal
sessions. A hook logs session events, a small site on 127.0.0.1 shows what is waiting on you
(one card per session that is, links resolved, pull-request state from `gh`), and `replay` and
`snapshot` report on historical transcripts and write resume commands to a file. How it decides
what is waiting, and what it reads and writes, is in [docs/design.md](docs/design.md).

## Install

```
cargo install --path . --locked
```

This puts `conn` in `~/.cargo/bin`. It needs Rust 1.99 or newer. `gh` (signed in) is optional and
gives the pull-request states; `niri` is optional and gives the "Open terminal" button;
`notify-send` is optional and gives the desktop notification.

## Configure

`~/.config/conn/config.toml` (or the file `CONN_CONFIG` names) says which Jira project keys, Jira
host and Slack host count as links in session text. Copy [docs/config.example.toml](docs/config.example.toml)
and replace its placeholder hosts and prefixes with your own. The file is read at launch. Where
there is no Jira, leave out the `[jira]` section (or leave it empty): no key becomes a link and the
work items page says Jira is not configured. Where there is no Slack, leave out `[slack]` (or leave
it empty) too: a Slack URL is then an ordinary link. `[reviews]` reads Slack, so it needs `[slack]`.

## Register the hook

```
conn hook --print-settings
```

prints the `hooks` fragment that registers this binary for `SessionStart`, `UserPromptSubmit` and
`Stop`, and `conn guard` as a `PreToolUse` hook before the Jira tools that change a ticket. Paste
it into `~/.claude/settings.json` yourself; `conn` never edits that file. The hook appends one
line per event to `events.jsonl` in the state directory; the guard is described under "Who owns
what". Neither ever fails the agent that runs it.

## Who owns what

When sessions share a ticket, a rollout or a follow-up, they say which of them owns it, so news
reaches the owner instead of whoever is nearest. A session runs these itself (it knows its id from
Claude Code's environment; `--session <id>` acts for another, from a terminal):

```
conn claim ABC-123                 # this session owns the ticket; also rollout:<name>, topic:<name>
conn claim --take ABC-123          # take it over from a session that may still run (when told to)
conn release ABC-123
conn who [ABC-123]                 # the owner, or everything owned, rolling out and waited on
conn wait rollout:checkout --then "re-run the Dev checks, move ABC-123"
conn wait --until 2026-11-06T09:00 --then "read the overnight export"
conn wait ABC-124 --then "rebase on it"   # until its owner releases it; also topic:<name>
conn wait --for "the vendor to pick a time" --then "apply the change"
conn wait --clear
conn rollout start checkout --ships ABC-123
conn rollout finish checkout --outcome shipped        # or rolled-back, abandoned; --note "..."
```

The **Owners** page ("Who owns what") opens with where the work collided: tickets and Slack
threads more than one session wrote to over the last week, or tickets written to by a session
that did not own them, read from the transcripts, with each write's time, session and first words.
A session that took part in one within the last day gets a red warning on its line. It then lists
every claim with its owner and the owner's wait, each
rollout with the tickets it ships and whom it reports to, every wait (a finished one marked "wait
over"), and the ledger's latest entries. Every other page follows it: inbox cards, session rows,
search results and the map's list say what each session owns and what it waits on; a session's own
page adds what it declared; the work items show each item's owner (an unowned one says so, with
the sessions that mention it folded beneath), and the map names a ticket's owner.

The hook also names each session's terminal: its title becomes its gist's headline (else its
directory's name), then ` · ` and the first block of its id, such as `Map labels · 0a1b2c3d`. The
id is what "Open terminal" finds the window by, so another `sessionTitle` hook would only fight it.

The hook tells every session how this works when it starts (and after a compaction), with what it
owns; and at each prompt, if it waits, what on, or that the wait is over and what it said it would
do next. Nobody has to relay "the rollout landed".

A claim on something another running session owns is refused with the owner's name, and so is a
second rollout of the same thing while one runs. A rollout that ships a ticket another session
owns is told to report to that owner and leave the ticket alone. The guard refuses a Jira comment,
transition, edit or worklog on a ticket another running session owns, and tells the agent whom to
send it to.

## Serve

```
conn serve
```

listens on `127.0.0.1:7357` (`--port` changes it) and prints one line: the sign-in URL,
`http://127.0.0.1:7357/?t=<token>`. Open it once (`--open` hands it to the default browser
through `xdg-open`); the server answers with a cookie and sends the browser to the inbox. The token is generated at each launch, so a restart prints a new URL and the
old cookie stops working. One `conn serve` runs per state directory: a second one started by
mistake exits at once, naming the process id of the one already running.

### What the inbox shows

One card per session that is waiting on you:

- a session **blocked** on a question, a permission prompt or another wait only you can end, shown
  first;
- a session whose latest turn **ended** with no later prompt typed by you, ranked by who started
  that turn (you, then a background task's notification, then the rest), oldest first.

A session's declared wait (see "Who owns what") moves a finished turn's card: up to just after
the blocked ones once what it waited on has come, and down, dimmed and marked "waiting", while it
still waits on something other than you. A blocked card stays first whatever its session waits on.

Each card opens with its **gist**: a line or two on what the session is building, fixing or
looking into, with a badge for what the card waits on (your decision, your move, waiting on
others, FYI). A tool-less Haiku run writes it once per card, about $0.002, while a conn page was
viewed in the last hour; the sessions page shows each session's latest gist on its row.

Each card shows the assistant's text as markdown, the links in it as chips (pull requests with
their state, Jira keys, Slack threads, documents), and buttons to open the session's terminal,
mark the card seen or pin the session. A path to a local `.md` file under the config's
`[files] roots` (a scratchpad brief, a plan) opens the file, rendered, in a pop-up.

With a `[reviews]` section, pull requests someone asked you to approve in the listed Slack
conversations are inbox cards too, marked apart from sessions and placed after the blocked ones.
The channels are read through Claude's Slack connector in a shut-in headless run (no shell or
file tools, no hooks, no transcript), about $0.003 a channel, every five minutes while a conn page
was viewed in the last hour. Each card is classified once per commit: fixed rules (CI, access
control, secrets, migrations, removed infrastructure, size) that a model cannot overrule, then a
tool-less model that may add flags; a card that needs a look says why in a highlighted box.
**Approve** approves the commit the card showed (a newer push is refused), then puts 👍 on the
message that asked. Every approval and its 👍 is in `reviews-journal.jsonl` in the state directory
and on the Approvals page, with Retry for a 👍 that failed.

The card you clicked last (to open its terminal, say) is outlined in orange, so coming back to the
page finds it again; each browser tab remembers its own.

The header's right side shows the plan's rate-limit windows (five hours, the week, and any
model's own weekly allowance) as small meters, amber from 75% and red from 90%, each followed by
when it resets (↻3h 5m for the five hours, ↻Fri 15:00 for a week). They are read once a minute while a conn page was viewed in the last hour, with
the token Claude Code keeps in `~/.claude/.credentials.json`, through `curl` (which must be
installed). When a read fails, the last numbers stay, dimmed, with the reason as their title.

**Seen** marks the card you were looking at, not the session: the button posts the moment that
card appeared. A seen card is dimmed and sorted after the unseen ones but is not removed, and the
session's next turn end is a new, unseen card.

An empty inbox says "Nothing is waiting on you" only when every source was read and every session
could be classified. Otherwise it says what was not read, and a session whose state could not be
told is counted ("N sessions could not be classified") with a link to `/sessions`, which shows the
reason for each.

### The map

The map page draws the active sessions as a dependency graph in three columns: the sessions, the
rollouts, then the tickets, topics and Slack threads. An edge is only something that matters:
what a session owns, the rollout it runs or waits on (green once the wait is over), the tickets a
rollout ships, a ticket it waits on, a ticket Jira says blocks another (read with `acli` for the
map's own tickets), and, in red, a ticket or thread it wrote to in the last day where another
session wrote too. A ticket two sessions merely mention is not drawn. A session's colour says what it is
doing (moving, on you, waiting on others, idle) and a ticket's where it stands (blocking,
waiting, moving, done); a ticket whose owner has ended is shaded. Click any node for a card with
its details (a session's gist, line and terminal button; a ticket's summary, owner, rollout and
writes; a rollout's state, what it ships and who waits on it). Active sessions tied to nothing
are listed under the drawing.

### Two tokens

The sign-in URL carries the **read** token, which becomes the cookie and admits reading pages. A
change (seen, pin, focus, refresh) also needs the **write** token, which appears only in the
rendered page and which the page's script sends as `X-Conn-Token`. It is never in a cookie or a
URL, so a request that carries only the cookie cannot change anything.

## Replay and snapshot

```
conn replay --since 2026-01-01
```

runs the same derivations over the transcripts under `~/.claude` (from that date, or all of them)
and prints hit-rate tables, so a rule is judged on real history.

```
conn snapshot --out ~/sessions.md
```

writes a markdown file of every session with a resume command, so a crash costs a few pastes. It
refuses a memory-backed location, where the file would not survive the crash.

## Development

`just check` runs the formatter check, clippy and the tests. `just install` puts the release
build at `~/.cargo/bin/conn`; a running `conn serve` keeps the old binary until restarted.

The tests include a leak check (`tests/no_leaks.rs`): no committable file, staged or in the
working tree, may hold your home path, your git email, an email address outside the reserved
example domains (`example.com`, `.test`, …), or a term listed in
`~/.config/conn/leak-denylist` (employer, host, project or channel names, one per line; `abc-`
matches the ticket keys `ABC-…`). The file must exist: create it empty if you have nothing to
list.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or conditions.
