# conn

A single-binary local tool for one developer running many Claude Code sessions: hook logger,
localhost site, transcript replay, snapshot. Product summary: `docs/design.md`.

## What never gets committed

This repository is public. Nothing personal, company-specific or machine-local enters it: not in
code, tests, fixtures, docs, file names or commit messages.

- **Personal data**: names, email addresses, usernames, home paths, account or user ids of any
  real person.
- **Company data**: employer, team, colleague, customer, repository, service, project, folder,
  host, channel and organisation names; real ticket keys and project prefixes; cloud
  organisation, folder and project ids; real transcript, Slack or Jira text.
- **Local configuration and personal workflow**: a developer's own hosts and prefixes, paths on
  their machine, how they land changes, their tooling preferences.

Where each goes instead: hosts, prefixes and ids in `~/.config/conn/config.toml`; terms to guard
against in `~/.config/conn/leak-denylist`; a developer's own workflow in their agent's memory or
user-level instructions, never in this file. Examples use `example.com`, `example.atlassian.net`,
`example.slack.com`, `example-org`, `ABC-123`, `/home/dev`. Fixtures follow
`tests/fixtures/README.md`.

`tests/no_leaks.rs` enforces what it can: the developer's home path and git email, email
addresses outside the domains RFC 2606 reserves for examples, and every term in the denylist
(optional; without it the test warns that names are not checked), in every committable file
and path, staged and in the
working tree. It cannot know a name nobody listed, so read a fixture or test string through before adding
it, and write a commit message with the same care (the test does not read it).

## Decisions

- **Language: Rust.** The tool must be a single-file executable serving a localhost site. Rust
  1.99, edition 2024. Library crate `conn` plus a thin `main.rs`.
- **Lint policy** is in `Cargo.toml` `[lints]`: clippy `all` and `pedantic` at deny, plus
  `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `todo`, `dbg_macro`,
  `as_conversions`, `wildcard_enum_match_arm`, `string_slice`; `unsafe_code` forbidden;
  `print_stdout`/`print_stderr` warn and are allowed (with a reason) only in `replay`, `snapshot`
  and the one sign-in line in `web`. Code never `unwrap`s outside tests, never indexes, never
  casts. `just check` must pass before work is handed over.
- **All HTML is escaped by `maud`.** `maud::PreEscaped` is banned (`clippy.toml`). A URL reaches an
  `href` only as a `SafeUrl`, whose one constructor accepts `https://` and nothing else.
  Assistant text is markdown, drawn by folding `pulldown-cmark` events into `maud` markup
  (`src/web/view/markdown.rs`); raw HTML in it is shown as text, never as HTML.
- **Third-party text is `Redacted` before a template sees it.** The view helpers take
  `&Redacted<String>`; the strings the model still holds plain are redacted through
  `fmt::redact`, the view's one call of `Redacted::of`.
- **Two per-launch tokens.** The read token is the sign-in URL's and the cookie's and admits
  reads; the write token appears only in the page's `data-write-token`, travels in
  `X-Conn-Token`, and admits changes. Their types differ (`Token<Read>`, `Token<Write>`).
- **Types are witnesses**: `SessionId`, `JiraKey`, `SafeUrl`, `Host` have private fields and one
  constructor. Parse at the boundary (hook stdin, transcript line, `gh` output, config); unknown
  values from Claude Code become `Unknown(String)`/`Other(String)`, never parse failures.
- **Errors**: each module has its own `thiserror` enum; `anyhow` only in `main.rs` and `cli.rs`.
  "Could not read or tell" (`Unreadable`, `Spawn`, `TimedOut`) is always a different variant from
  "read it and it was X" (`Malformed`, a value).

## The pending rule

A session is pending when its latest turn end has no later human-typed prompt in that session,
whoever started the turn. The trigger ranks (`Human` above `TaskNotification` above the rest) and
never filters.

## Handing work over

Say what ran and what did not: which tests, which were mutation-checked and how, what remains
unverified. Never report unverified work as verified.
