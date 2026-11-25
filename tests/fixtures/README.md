# Fixtures

Fixtures are real record shapes with no real content.

- Keep the structure exactly as Claude Code wrote it: field names, nesting, record kinds, ordering,
  value types, timestamps' format. A fixture that is tidier than reality tests nothing.
- Replace **all free text**: prompts, assistant messages, tool inputs and outputs, titles, file
  contents. Use obviously synthetic text ("Placeholder assistant message.").
- No email addresses, no hostnames (use `example.com`, `example.atlassian.net`,
  `example.slack.com`), no usernames or home paths of a real person (`/home/dev/proj`), no ticket
  keys from a real tracker (use `ABC-123`), no real repository, channel or organisation names.
- Session ids and timestamps are regenerated; they must still parse as what they are.
- Before adding a fixture, read it through once, top to bottom; masking by search-and-replace alone
  misses what it does not search for.

## `replay/claude`

A fake Claude directory for the replay tests: five sessions of a few records each, written so
the replay tables can be derived by hand (the derivation is in `src/replay/report.rs`). Records
carry only the fields the reader looks at. `c3` ends before the test's `--since` date and must
not be counted; `b2` has one line that is not JSON.
