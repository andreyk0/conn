# conn: design

`conn` helps one developer who runs many Claude Code terminal sessions at once and loses track of
which are waiting on them, and of which session owns what they share. It is one executable with
these subcommands.

- **`conn hook`** runs as a Claude Code hook (`SessionStart`, `UserPromptSubmit`, `Stop`). It reads
  one JSON payload on stdin and appends it, with the received-at time, to an append-only JSONL
  event log, and answers a start or a prompt with the session's title (its gist's headline, then
  ` · ` and the first block of its id, which "Open terminal" matches the window by) and its place
  in the coordination ledger. It is fast and never fails the agent that runs it.
- **`conn serve`** serves a small server-rendered HTML site on 127.0.0.1. It shows one pending card
  per session that is waiting on the human, opening with the session's gist and whom the card
  waits on, with the links in the card's text resolved (Jira keys, Slack permalinks, GitHub pull
  requests, document URLs) and pull-request state from the `gh` CLI. Each card offers an "open
  terminal" action (window title match under the niri compositor) or a resume command. Dormant
  sessions (process gone, transcript on disk) are listed with resume commands, and the open ones
  are saved, automatically and on request, to resume after a crash or a reboot. Beside the inbox:
  a map of the active sessions as a dependency graph of what they own, run, wait on and collided
  over; the user's open Jira work items with the session that owns each;
  who owns what across the sessions; pull requests someone asked the user in Slack to approve (as inbox cards, approvable in
  one click) and the approvals made that way; a search over the prompts the sessions hold; and,
  where configured, the follow-ups: what is due, who should act on it and the prompt to hand
  them; and, where configured, the GCP PAM entitlements the user may request, each with the
  grant held and its time left, requested in one click for a length PAM accepts (break-glass
  ones listed apart).
  `--open` hands the sign-in URL to the default browser.
- **`conn replay`** runs the same derivations over the historical transcripts and prints hit-rate
  tables, so a rule is judged on real history before it is trusted live.
- **`conn snapshot`** writes a markdown file of sessions and their resume commands, so a crash costs
  a few pastes.
- **`conn claim`, `release`, `who`, `wait`, `rollout` and `defer`** are run by the sessions
  themselves to keep the coordination ledger, and **`conn guard`** runs as a `PreToolUse` hook on
  Jira writes to hold them to it. See "The coordination ledger".

## The coordination ledger

Sessions that share a ticket, a rollout or a follow-up step on each other when nothing says which
of them owns it: two sessions moving one ticket in the same minute, news reaching the wrong one,
two rollouts of one repository finding each other through git. None of this can be read out of
the transcripts reliably (whether a session waits on a rollout, and what it means to do after, is
prose), so the sessions declare it, in `ledger.jsonl`:

- **Claims.** `conn claim <subject>` makes the session the owner of a ticket (`ABC-123`), a
  rollout (`rollout:<name>`) or a named piece of follow-up (`topic:<name>`). A subject has one
  owner. Claiming one that a session which may still be running owns is refused, naming the
  owner, unless `--take`; one whose owner has ended passes over and records from whom.
- **Waits.** `conn wait` says what an idle session is idle for: the next finish of a rollout, a
  ticket or topic until it is released (its owner's part is done), a time, or something said in
  words, and, always, what it does next. One wait per session; a new one replaces it. A wait on a
  ticket draws the chain on the map: the waiting session, the ticket, its owner. A release is a
  rollout, even one only the user can run, so a session waits on `rollout:<name>`; a `--for` wait
  whose words name a rollout the ledger knows ("ship CHECKOUT to Prod" names `checkout`) is kept, and the
  answer gives the `conn wait rollout:<name>` to run instead. A wait on a ticket, or a time or
  words wait about one (`--ticket ABC-123`), is also over once Jira resolves the ticket (Done,
  Won't Do, any resolution), whoever closes it: `conn serve` reads the resolution of each ticket
  a standing wait names and saves the closed ones, so the hook tells the waiting session too.
- **What a card waits on.** While a session's wait lasts, its finished turn sits below the cards
  that need the user now, and the card's badge says the wait instead of the gist's reading of the
  card, since the session declared it: "Waits on release: checkout" for a release nobody runs (often
  the user's to start), "Release running: checkout" once someone runs it, and "Waiting on:
  <what>" for any other wait. The map colours such a session the same way (below).
- **Rollouts.** `conn rollout start <name> --ships ABC-1` and `conn rollout finish <name>
  --outcome …`. A second start while a run is under way in a session that may still be running
  is refused. A wait on a rollout is over at its first finish after the wait began. A rollout
  that ships a ticket another session owns is told to report to that owner and leave the ticket
  to it: release sessions roll out; the session that owns a ticket moves it.
- **Deferrals.** `conn defer ABC-1 --until <time> --check "<what to check then>"` says a
  shipped ticket's exit criteria cannot be checked before that time: a week of clean metrics, a
  month-end run. One per ticket; a new one replaces it, and one for a time already past is
  refused, since the check is due. Written by the session that ships the ticket or by a close
  check that finds it is too soon, so a ticket costs at most one early check.
- **The guard.** `conn guard`, registered before the Jira tools that comment on, transition, edit
  or log work on a ticket, refuses the call when another session that may still be running owns
  the ticket, and tells the agent who it is. It lets through anything it cannot read or decide:
  it stops a known collision and must never be the reason a session cannot work.

- **Reminders.** `conn hook` prints, for `SessionStart`, how the ledger is used and what the
  session owns, and for `SessionStart` and `UserPromptSubmit`, the session's wait: still on, or
  over with what it said it would do next. Claude Code adds that text to the session's context,
  which is how a waiting session learns its rollout landed without anyone relaying it. `Stop`
  gets nothing.

- **Collisions.** What sessions declare is compared with what their transcripts say they did.
  Each successful Jira comment, transition, edit or worklog, and each Slack post, is read from
  the transcript's tool calls (a refused or failed call is not a write). Over the last week, a
  ticket or Slack thread that two sessions or more wrote to, or a ticket written to by a session
  that did not own it at the time, is a collision: the Owners page lists each with its writes,
  and a session's line warns of one it took part in within the last day. Posts to a channel
  outside a thread are left out.
- **On the pages.** Wherever a session is shown (an inbox card, a session row on the sessions and
  dormant pages, a search result, the map's list), a highlighted line says what it owns and what
  it waits on; the session's own page adds what it declared, newest first. Wherever a ticket is
  shown (the work items, the map's anchors and their tips), its owner is named. The Owners page
  holds the whole ledger.

"May still be running" is the registry's verdict: a process that cannot be checked counts as
running, so nothing is taken from a session that may be working. The commands that write take an
exclusive `flock` on the ledger from their read to their append, so two sessions claiming one
ticket at once are decided in turn.

## The pending rule

A session is pending when its latest turn end has no later human-typed prompt in that session,
whoever started the turn. The turn end is the later of the event log's last `Stop` and the
transcript's last assistant turn end; the prompt is the later of the event log's and the
transcript's last human-typed prompt. A turn end at the prompt's own instant is not after it.

A turn started by a scheduled wake-up, a peer session or a coordinator is still waiting on the
human once it ends. The trigger therefore ranks and never filters. Cards sort by tier, then oldest
first:

1. **Blocked**: a question the session asked, or a wait only the human ends (a permission prompt,
   a sandbox request, an open dialog).
2. **Quota hold**: blocked until the usage quota resets; nothing the human does ends it, so it
   comes after every other blocked card and raises no notification.
3. **Human**: the turn the human started.
4. **Task notification**: the turn a background task's completion started.
5. **Other**: every remaining trigger, including ones this code does not recognise.

A card the human marked seen sorts after every unseen card and is dimmed, not removed. A pin
does not move a card; the sessions and dormant lists lead with pinned sessions, the earliest pin
first.

## What a session is doing

[`classify`](../src/pending/mod.rs) is the one function that turns the evidence about a session
into an activity and, for some, a card. The first line that applies decides. A session whose
liveness cannot be told is `Unknown` before any line is read.

1. Live, with an unanswered `AskUserQuestion`: **Blocked** on input, card with the question.
2. Live, and the supervisor's listing says the process is waiting: **Blocked** on what it reports,
   card naming the wait.
3. Live, and the latest turn end is after the last human prompt: **Watching** when a task listed by
   the `Stop` hook is still running or the listing says the process is busy, else **Idle**; card
   with the turn's text. When the listing could not be read but the process is alive, this line
   still applies and the activity says the listing was unavailable.
4. Live otherwise: **Running**, no card.
5. Not live: **Dormant**, no card, resumed with `claude --resume`. When the supervisor still
   lists it as a background session, its state is said beside it ("background session is
   done"); whether it is listed, or the listing could be read, changes nothing else. When a
   complete walk of the projects directory found no transcript for it, it is **Gone** instead:
   it is listed with the dormant sessions, says there is nothing to resume (and, for a
   background session, that `claude rm` removes its record), and offers no resume command. A
   transcript not found while the walk is incomplete leaves it Unknown.
6. Anything else is **Unknown** with its reason: the listing failed for a live process, the
   process table could not be read, the sources disagree, a transcript tail could not be
   followed, a state this code does not know. An Unknown session has no card and is counted
   wherever the page speaks of calm.

Every session row and inbox card has a pushpin in its top right corner that pins and unpins the
session: grey while it is not, yellow while it is. Pinned sessions head the session lists in pin order,
and the Dormant page lists its pinned sessions under their own heading, all of them, so a session
pinned and closed on Friday is first on that page on Monday, with its resume command. The map
draws pinned dormant sessions beside the live ones, marked pinned, however long ago they ran.

## A card's gist

Each card opens with a line or two saying what its session is building, fixing or looking into,
and what the card waits for: **your decision**, **your move**, **waiting on others**, or **FYI**
(it only reports). Where the card itself says, a rule decides the wait: a question is a decision,
a permission prompt or another harness wait is a move, a quota hold asks nothing. For a finished
turn the model's reading is used, and only for the card it was written for. The sessions page
shows each session's latest gist on its row, and its filter finds a session by it.

A tool-less Haiku run (`claude -p --tools "" --strict-mcp-config`, no hooks, no transcript)
writes the gist once per card, from the session's latest eight prompts, the card's text and the
session's previous gist. It is told to keep the previous gist word for word while it still fits,
so the line follows the work as it moves and does not churn while it stays. Runs happen only
while a page was viewed in the last hour, two at a time; a failed run is tried again after ten
minutes, and the card shows the failure beside the earlier gist, if any. The gist is redacted,
escaped and cut to 200 characters; the run has no tools, so the text it reads can only change
what it writes.

## Replying from a card

A finished turn's text can be answered line by line without leaving the page. Composing is a
mode the human switches the card into with its **Reply** button; until then a click on the text
does nothing but what it would anywhere. In the mode the card's text opens in full and every
block of it (a paragraph, a list item, a heading, a code block, a table row) shows a ↩ that
opens a reply under that block. The message quotes each answered block on one line (an ordered
item led by its number, cut at 160 characters), in the text's order, with the reply under it;
a reply left empty says "This one.", which picks that line; the general text comes last.

**Copy & open terminal** copies the message and raises the session's terminal, as "Open
terminal" does; the human pastes it and presses Enter. A session with no terminal to raise
gets **Copy reply**, to paste after resuming it. The server takes no part: nothing is sent to
a session, so who can make a session act is unchanged.

Only the turn the session waits on takes a reply (its pending card's turn; on the session's
own page, that one turn end and no older one), and an open question's card: there each option
is a block quoted by its label, and a ↩ left empty picks it. Picks with nothing written copy
nothing: the terminal is raised and the hint names the options to choose in its dialog, since a
paste there becomes the question's typed answer. With anything written, the message is copied
and pasted as that typed answer. A harness prompt (a permission, a sandbox request) is answered
in the terminal. The draft is kept in the tab's session storage under the session and the
instant its turn ended or its question was asked, so a reload brings it back and the next one
starts clean; the page does not reload while a card is in the mode.

## Follow-ups

With a `[followups]` section, the Follow-ups page lists what is due and who is best placed to
act on it. Every row is a rule over what conn already reads, never a model's reading, most
pressing first:

1. **Someone asked**: a listed work item has comments by others after the user's latest, whatever
   they marked seen (seeing a question is not answering it).
2. **A wait is over** and its session is not at work: idle, dormant, or gone.
3. **A release is pending**: sessions wait on the next finish of a rollout nobody runs.
4. **An owner is gone**: a topic, or a listed work item, claimed by a session with no process and
   no transcript. A dormant owner is not due by itself: it can be resumed when something
   happens. An item waiting for its close check is not listed for it: the check takes it over.
5. **Close checks**: one sweep over the listed work items in a status the config's
   `close_statuses` names that are ready for their check. One is not ready while a running
   rollout says it ships it (still going out), while a session that may be running owns it (it
   holds the context and closes it itself; the guard would refuse the sweep's writes), or while
   its deferral's time has not come. A dormant or gone owner does not hold it: its claim passes
   to the sweep. One session checks each item in turn, closes what holds, records `conn defer`
   for what can only be checked later, and leaves an item another running session owns to that
   session; an item whose deferral is over comes with what was to be checked. One session per
   sweep, not per item, because each check is short and a session's start is not. The items not
   ready are listed under "Not yet" with why, outside the count.

A ticket's other reasons share one row. Each row names who acts and hands them a prompt the one way that
reaches them: the owning session's terminal (copied, the terminal raised), the owning session's
resume command with the prompt, or a new session's command in the directory of a session that
named the ticket, told to `conn claim` it first. A pending release has no prompt: a person starts
it. The nav counts the rows. Like the reply composer, the page starts nothing.

## The work map

The map page draws the active sessions as a dependency graph. Two sessions mentioning one ticket
say little about who depends on whom, so mentions are not edges; an edge is only what bears on
who does what next:

- **owns**: a session's claims in the coordination ledger;
- **runs** and **waits on**: a session's rollout, and a session's wait on one (green once over);
- **ships**: a running rollout to each ticket it said it ships;
- **collided**: a session to a ticket or Slack thread it wrote to within the last day where
  another session wrote too, or while another owned it (see "The coordination ledger");
- **blocks**: a ticket to one its Jira links say it blocks ("Blocks", "Depends On"), while
  neither is done, and one end is on the map. Read for the map's own tickets (claimed, shipped,
  collided) and for the work items with a blocking link, one ticket at a time with
  `acli jira workitem view`, since the search refuses the links field; a ticket it
  blocks is marked blocked, and the blocker sits beside it, so a chain reads across: a session,
  the ticket it owns or waits on, the ticket blocking that, and that ticket's owner.

Three columns, the sessions, the rollouts, then the tickets, topics and threads; the sessions keep
the inbox's order, grouped so that sessions sharing a thing sit together, and each thing sits at
the mean row of what points at it, so most edges run straight across. A session's colour is its
**motion**: blocked on the user (a question or a prompt, a card asking a decision or a move, a
background run stopped or failed), waiting on the user's release (its wait is on a rollout
nobody runs), waiting on others (its card or its wait says so), moving, idle. A ticket's
is its **progress**, read as before from what the sessions link: a pull request's checks and
reviews, a work item's status name. A thing whose owner has ended is shaded: nobody is on it. A
rollout sessions wait on that nobody runs is a **pending release**, drawn with how many wait in
the release colour its waiting sessions share: the work held until someone releases it, the
user's move but not an urgent one. Its card, the Owners page and the
Follow-ups page say the same, and a rollout only waited on, never started, is listed too.

The page is server-rendered SVG with no script. Each node links to its detail card, which the
style sheet shows while the URL's fragment names it: a session's gist, ledger line and terminal
button; a ticket's summary and status, owner, the rollout shipping it and its writes; a
rollout's state, what it ships and who waits on it. Active sessions tied to nothing are listed
under the drawing.

## Work items

With a `[jira]` section (a host and the project prefixes), the work items page lists the user's
open Jira work items, read with `acli jira workitem search`, by status section (In Progress, To
Do, then by name), each with the session that owns it in the coordination ledger; an item no
session owns says so, with the sessions whose text names its key folded beneath. The list is kept in
`jira-work-items.json`, so a restart shows the last good list at once; it is read again when it is
ten minutes old or the user asks, and a failed read is shown beside the list it did not replace.
Without the section there is no Jira and no page.

An epic (a type at Jira's hierarchy level 1 or above, whatever its name) is tinted and tagged,
with how many of its children are done out of all (`2/5` and a bar), where the map's tip shows
it too; its children are read with `acli jira workitem search --jql "parent = <epic>"`, since
the search refuses the `parent` field. Within a status section an epic's items sit under it like
files under a directory, one level deep; items of an epic in another status, or not assigned to
the user, sit under a line naming that epic. Each item's epic comes from the per-item view the
comments are read with (`--fields key,comment,parent`), so it costs no call of its own and
follows the same changed-item asks; an item not viewed yet sits at the top. An item that waits on an open ticket by a "Blocks" or
"Depends On" link recedes to the end of its section and names the tickets it waits for: one
search finds the user's open items with such a link, and each one's links are read as the map's
are, so a link to a done ticket does not count. An item whose links are not read yet reads as
ready; the health page says how the reads stand.

A listed item wears a badge counting the comments the user has not seen, wherever a page shows
its key (the work items table, link chips, the owners page and "owns" lines, the map). A comment
is unseen when someone else wrote it after the later of the user's own latest comment on the
item, since replying means they read the thread, and the item's "seen" mark. Marking a card seen
marks the unseen comments the card showed on the tickets its session owns, up to the newest
one's time as the page showed it, so a comment that arrived since stays unseen. An item no
session owns is cleared only by a reply. A thread Jira returns only part of shows "?", since its
latest comments are not known.

The search refuses the comment field, so comments are read one item at a time with `acli jira
workitem view`. Every two minutes the reader asks the search which listed items changed (JQL
filters on `updated` though the search will not return it), looking two minutes further back
than its last ask, and reads those and any never read, at most twenty a minute. The first pass
reads every listed item once. The reads are kept in `jira-comments.json`, with the commenters
recorded only as the user or not, so a restart does not read them all again. The user's account
id is the assignee of the items a `currentUser()` query returns.

## Saved sessions

A crash or a forced reboot ends every session at once; the Sessions page keeps what was open so
each can be resumed after. A save holds each session's title, gist, directory, last activity and
resume command, the command made while the session ran, since only a live process tells the
launch flags it repeats.

- **The automatic save**, one slot, follows the open sessions. A session that starts running
  joins at once: no crash starts one. One that stops leaves only after a minute, and only while
  `systemctl is-system-running` answers `running` or `degraded`: a shutdown stops the sessions,
  then conn, within that minute, and systemd answers `stopping` meanwhile, so the save keeps
  what the reboot took. A crash writes nothing new. A session that cannot be classified neither
  joins nor leaves, and nothing changes until a read of the listing, the registry and every
  transcript is complete, since a partial read makes running sessions look stopped.
- **On a new boot** (`/proc/sys/kernel/random/boot_id` differs from the save's, or either cannot
  be read), the old automatic save is kept as a "before reboot" save and the automatic save
  starts over from what runs.
- **Kept saves**: "Save now" keeps the running sessions; at most ten, newest first, an eleventh
  pushes out the oldest; each has a delete.

A save's page lists its sessions with the command that resumes each, marking the ones running
again. Saves live in `saves/` under the state directory: `auto.json` and one `<millis>.json` per
kept save; a file that cannot be read is listed and left alone.

Ending a session with `/exit` is not told apart from closing its terminal: either leaves after
the minute. Claude Code's `SessionEnd` hook says `prompt_input_exit` for the first, and would
let it leave at once; it is not read.

## Review requests

With a `[reviews]` section naming Slack channels, pull requests linked in those channels' top-level
messages of the last two days that still wait on the user (open, written by someone else, not yet
approved by the user at their current commit) are inbox cards, after the blocked sessions. Review
requests are immediate: an older one has been answered, given to someone else or asked again, and
the new ask is a new message. A channel is read through the claude.ai Slack connector in a
headless `claude -p` run that may call only the read tool, with the sending tools denied; what the
tool returned is read from the run's event stream, never from the model's reply, and a run that
did not call the tool is a failure, never an empty channel. A channel is read 25 messages a page,
at most four pages, because Claude Code replaces a tool result over its size cap with a note
about it; an answer that is not a message list, that note included, is a problem the page names,
never an empty channel. Each pull request is then read with `gh`. The queue is read every five minutes while a
page was viewed in the last hour; at most forty pull requests per read.

Each request is classified once per commit. Rules over its files and diff flag what always
deserves a look (CI workflows, CODEOWNERS, access control, secrets, migrations, removed
infrastructure, destroyable data, grants to anyone, more than 400 changed lines); then a tool-less
Haiku run reads the diff and may add flags. A rule's flag stands whatever the model says, and a
model that could not answer leaves the request unclassified, never routine.

**Approve** approves the commit the card showed and nothing newer: the request is read again, and
a new commit, or a pull request that no longer waits, refuses the click. The approval is
`gh pr review --approve` as the user, recorded in the append-only `reviews-journal.jsonl` before
the outcome is shown; then a 👍 is added to the Slack message that asked, in the background, and
that outcome is journalled too. The approvals page lists the journal, with Retry for a 👍 that
failed.

## Sources and precedence

Three sources describe a session's process, each possibly absent; the transcript, Claude Code's own
file and read-only, is always present for a known session.

- **The registry and `/proc`**: an entry written by a process, checked against the process table.
  It is the direct observation of liveness and wins when it has an opinion: alive or gone, whatever
  the listing says.
- **The supervisor's listing** (`claude agents --json --all`): what the process is doing (waiting,
  busy) and background sessions. When it says there is a process and the registry has no entry, the
  session is live. When neither source can be read, liveness is unknown. A failed attempt is
  covered by the last good listing for a minute, and the health line still reports the failure.
- **The hook event log**: sees a turn end or a prompt the moment it happens; the transcript sees it
  a moment later. The builder takes the later of the two for the prompt and for the turn end.

## Health and "never calm"

Every source is read, failed or not yet read; the model carries each state with its time, and a
failure is never only logged. The model's health holds the listing, the registry, the event log and
its lines without a usable event, transcript discovery, the count of transcripts found, passed over,
unreadable and with lines that yielded no record, the scan progress, the transcripts whose tail
could not be followed on the latest poll, the pull-request readings (fresh, not found, stale,
failed, not asked yet), `seen.json`, links that could not be written as a URL, listed sessions without an id, the
desktop notification, the Jira work items and the review queue where they are configured, the gist
runs, the plan usage read, the coordination ledger, and the number of sessions that are `Unknown`.
The work items, the review queue, the gists, the ledger and the plan usage are not what the inbox's calm depends
on, so the header's health line does not list them; the health page does. The usage gauges in
the header keep the last numbers read when a later read fails, dimmed, with the failure as their
title.

**Calm is never shown by default.** An empty inbox says "Nothing is waiting on you" only when no
source failed or was never read, the transcript scan is complete, no transcript tail is at fault,
no gh reading failed outright, the notification daemon works, and no session is `Unknown`.
GitHub answering that a pull request does not exist (or is not visible to the account) is an
answer, not a failure: the chip says "not found" and the header stays calm, since a placeholder
link in a session's text would otherwise hold it for as long as the session is listed.
Otherwise the page lists exactly what is missing. The header's health line and the inbox's empty
state name the unclassified sessions ("N sessions could not be classified") and link to the
sessions page, where each shows its reason; the health page has a row for every field.

## Text from outside

Transcript text, titles, questions, error text from `gh` and every other string that comes from
outside is untrusted in two ways, and each has a witness.

- **`Redacted`** is text in which no credential of a known shape is left (access keys, API and
  GitHub tokens, bearer credentials, JWTs, private keys, environment assignments whose name says
  secret). Its one constructor applies the patterns. Assistant text is redacted where it is read from
  the transcript or the hook, so nothing downstream holds the unmasked text; the few strings the
  model still holds plain are redacted once at the view boundary, and every view helper takes
  `Redacted`. Redaction is best effort and the page's footer says so.
- **`SafeUrl`** is an `https://` URL with a non-empty authority, no userinfo and no whitespace,
  control or backslash characters. It is the only way a URL becomes an `href`. A link whose
  destination is refused is shown as its text, and the number refused is carried to the health line.

Assistant text is markdown. The renderer folds a markdown parser's event stream into `maud` markup
directly: headings (cut off at the fourth level, below the card's own title), paragraphs, emphasis,
code, lists, task lists, block quotes, tables, rules and breaks. Raw HTML in the text is shown as
text, an image is its alt text, and no string of HTML is ever written, so `maud::PreEscaped` is
banned. Text runs are redacted again after the parser decodes entities and escapes, and pass
through the link extractor, so Jira keys, Slack permalinks and pull-request references in prose
become anchors. A list continuing a key (`ABC-123, 134 and 345`, `ABC-581/694`) links each bare
number under the key's prefix, when it has as many digits as the key's own and is not part of a
date, count or version; a bare `#123` is always a pull request.

An absolute path to a `.md` file under the configured `[files] roots`, in prose or as a whole code
span (the way agents write a scratchpad brief), opens the file in a pop-up, with a button to open
it in a window of its own. It is not a link in the model's sense: no chip. The server reads such
a file only at `/file?path=`, behind the read cookie, and only when the path
as written lies under a root (checked before the file system is asked anything) and the path its
symbolic links resolve to is still a `.md` file under a resolved root, a regular file, UTF-8 and at
most 2 MiB. Its text is redacted and drawn by the same markdown renderer.

## Access

The server binds 127.0.0.1 only. Two tokens are generated at each launch, each of its own type, so
one cannot be offered where the other is required.

- The **read** token is printed in the sign-in URL and becomes the cookie
  (`HttpOnly; SameSite=Strict`). It admits reads.
- The **write** token is rendered into each page's `data-write-token` attribute and sent by the page's
  script as `X-Conn-Token`. It is in no cookie and no URL.

Admission is one function that decides in this order and says why it refused: `Host` is a loopback
name with the server's port (400); the cookie is the read token (401); a change also carries the
server's own `Origin` (403) and the write token in the header (403). The read token in the header
of a change is refused. Comparisons take the same time wherever the texts differ. Every response
forbids caching, framing and sniffing, and its content security policy allows the page its own
style sheet and scripts and its own origin and nothing else. A change is a form post whose fields are parsed into a
typed action at the boundary; a field that fails names itself in the 400.

## What it does to the world

The site reads and shows. It never sends input to, resumes or kills a session. It acts on the
world in one way only, on the human's click: approving a review request, which approves the pull
request on GitHub and adds a 👍 to the Slack message that asked (Retry adds a 👍 that failed).

- **Files it writes.** The event log (`events.jsonl`, append-only, by the hook); `seen.json` (only
  from the human's explicit actions, rewritten whole and never replaced when it could not be read,
  which is shown as an error on the page); `reviews-journal.jsonl`, the approvals, append-only;
  `jira-work-items.json`, `jira-comments.json` and `gists.json`, derived caches replaced when they cannot
  be read; `ledger.jsonl`, append-only, by the sessions' own ledger commands; `serve.lock`, which
  holds the serving process's id; `saves/`, the saved sets of sessions (the automatic one
  rewritten as sessions start and stop, kept ones only from the human's clicks and on a new
  boot); the snapshot file named by `--out`.
  The state directory is created private. The map's node positions are kept in the browser's
  local storage, and the inbox card last clicked and reply drafts in the tab's session storage,
  not by the server.
  Transcripts and the registry are Claude Code's and are only read.
- **Processes it runs.**
  - `claude agents --json --all` for the listing, every ten seconds.
  - `systemctl is-system-running`, only while a session that stopped a minute ago waits to
    leave the automatic save.
  - `gh pr view` for pull-request states, two at a time and only those that are due.
  - `acli jira workitem search` for the work items, when the list is ten minutes old or the human
    asks; while a page was viewed in the last hour, one at a time and at most every ten minutes
    each, `acli jira workitem search` for the work items with a blocking link,
    `acli jira workitem view` for the links of those and of each map ticket, and
    `acli jira workitem search` for each open epic's children; and, while a page was viewed in
    the last hour, `acli jira workitem search` every two minutes for the listed items that
    changed and `acli jira workitem view` for their comments, at most twenty a minute.
  - For the review queue, every five minutes while a page was viewed in the last hour:
    `claude -p` with the Slack connector's read tool alone, once per channel; `gh api user`,
    `gh pr view` and `gh pr diff` per pull request; `claude -p` on Haiku with no tools, once per
    commit, to classify it.
  - On an approval click: `gh pr review --approve`, then `claude -p` with the connector's
    reaction tool alone; on a Retry click, the reaction run again.
  - `claude -p` on Haiku with no tools, once per card, for its gist.
  - `curl` to Anthropic's OAuth usage endpoint for the plan's rate-limit windows, once a minute
    while a page was viewed in the last hour. A refusal as too frequent (HTTP 429) backs off:
    each one in a row doubles the wait, up to sixteen minutes, and a longer `Retry-After` is
    honoured up to an hour; a successful read restores the minute.
  - `niri msg` to find a window and to focus it, on the human's click; `notify-send` with a count
    and no session text, at most one a minute; `xdg-open` with the sign-in URL under `--open`.
- **Network.** None of its own. `gh` talks to GitHub and `acli` to Jira, with the person's
  credentials. `claude -p` reaches the person's claude.ai connectors and the model: a card's
  redacted prompts and text go to the model that writes its gist, and a requested pull request's
  title and diff to the model that classifies it. `curl` sends Claude Code's own OAuth access token, read from its
  `.credentials.json` at each read, to `api.anthropic.com` and nowhere else; the token goes to
  `curl` on stdin, never as an argument, and an expired one is left for Claude Code to renew,
  since refreshing it would spend the single-use refresh token a running session relies on.

**The boundary.** The poller asks GitHub, through `gh` and with the person's credentials, about
every pull-request reference found in the text of the sessions that are not dormant and in the inbox's
cards. Text a transcript carries therefore decides which repositories are queried: a session that
mentions a pull request in a repository the person can read gets that pull request's title,
state, review and checks fetched, whoever wrote the mention. The query is `gh pr view` with the
owner, repository and number parsed into a typed reference, so a reference cannot inject arguments,
and the response is shown, never acted on. Likewise anyone who can post in a configured channel
decides which pull requests are read and classified; a request becomes an approval only by the
human's click, of the commit the card showed.

## Invariants

- **Nothing acts on a session, but the guard.** The tool can open a terminal window or print a
  command. The one exception is `conn guard`, which refuses a Jira write to a ticket another
  running session owns, with the owner's name as the reason; it refuses nothing else, and lets
  through whatever it cannot decide.
- **One outward action, and only by a click.** Approving a review request, with its 👍, is the
  only thing the tool does to anything outside this machine, and nothing but the human's click
  starts it.
- **Everything on disk is derived or append-only.** The seen and pinned marks are the human's;
  the event log, the approvals journal and the ledger are append-only; the work items and the
  gists are caches rebuilt by reading Jira and running the model again.
- **One server per state directory.** `conn serve` takes an exclusive `flock` on `serve.lock`
  before it reads or writes anything, and a second one is refused, naming the first one's process
  id; the kernel drops the lock when the process exits, however it exits. The hook never takes it,
  since many sessions' hooks append to the event log at once.
- **Loopback only, two tokens.** A web page in the person's browser cannot drive the server.
- **No company identifiers.** Hosts and project prefixes come from `~/.config/conn/config.toml`;
  nothing identifying is written into the repository, its tests or its fixtures.
