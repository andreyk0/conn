//! Nothing personal, company-specific or machine-local is committed. Every file `git` would
//! commit is scanned twice, as staged (the blob `git commit` takes, a symbolic link's target
//! text included; a submodule by its path alone) and as in the working tree (tracked, and
//! untracked but not ignored), along with its path, for
//!
//! - the developer's own identifiers: the home directory's path, also in the form Claude Code
//!   names project directories by (`/home/jdoe` as `-home-jdoe`), when it has at least two
//!   components (`/` and `/root` name no one); and the emails `git var` says commits are
//!   authored and committed with;
//! - any email address outside the domains RFC 2606 reserves for examples;
//! - each line of the developer's denylist, `$XDG_CONFIG_HOME/conn/leak-denylist` (else
//!   `~/.config/conn/leak-denylist`): employer, colleague, host, project and channel names
//!   (without a channel's `#`), one per line; a line starting with `#` is a comment. It lives
//!   outside the repository because its contents are exactly what must not be in it. It must
//!   exist: a missing one fails the test, saying how to create it, and an empty one is a
//!   developer's statement that they have nothing to list.
//!
//! Text and terms are compared folded: lower-cased, every run of whitespace one space, so a name
//! split across a line break still matches. A term matches wherever no letter or digit adjoins
//! it; an edge of the term that is not a letter or digit is not checked, so the key prefix
//! `abc-` matches `ABC-123`. Folding is `to_lowercase`, not full Unicode case folding: `ß` does
//! not match `SS`. Bytes that are not UTF-8 are read lossily, so UTF-16 or compressed text is
//! not searched. Commit messages are not read: they are the committer's to check.
//!
//! Anything that cannot be read fails the test, since "could not read it" is not "it is clean".

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;

use folding::{Folded, Place, Source, Term};

/// Folded text and the terms searched in it. A term is folded by the one function text is, and
/// only [`Term::parse`] makes one, so no unfolded string can be searched for.
mod folding {
    use std::num::NonZeroUsize;

    /// Text folded for matching: lower-cased, each run of whitespace one space, with the line
    /// of the raw text each byte came from (`lines.len() == text.len()`).
    pub(super) struct Folded {
        text: String,
        lines: Vec<NonZeroUsize>,
    }

    impl Folded {
        pub(super) fn of(raw: &str) -> Self {
            let mut text = String::with_capacity(raw.len());
            let mut lines = Vec::with_capacity(raw.len());
            let mut line = NonZeroUsize::MIN;
            let mut in_space = false;
            let mut push = |text: &mut String, c: char, line: NonZeroUsize| {
                text.push(c);
                lines.resize(text.len(), line);
            };
            for c in raw.chars() {
                if c.is_whitespace() {
                    if !in_space {
                        push(&mut text, ' ', line);
                        in_space = true;
                    }
                    if c == '\n' {
                        line = line.saturating_add(1);
                    }
                } else {
                    in_space = false;
                    for lower in c.to_lowercase() {
                        push(&mut text, lower, line);
                    }
                }
            }
            Self { text, lines }
        }

        pub(super) fn text(&self) -> &str {
            &self.text
        }

        /// The raw line the folded byte at `offset` came from; `offset` is a match's start, so
        /// it lies inside the text.
        pub(super) fn line_at(&self, offset: usize) -> Place {
            Place::Line(self.lines[offset])
        }

        /// The byte offset of every occurrence of `term` with no letter or digit adjoining it,
        /// at an edge where the term itself is a letter or digit. Every start is tried, so a
        /// rejected occurrence cannot hide an overlapping one.
        pub(super) fn word_starts(&self, term: &Term) -> Vec<usize> {
            let term = term.folded.as_str();
            let is_word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
            let check_before = is_word(term.chars().next());
            let check_after = is_word(term.chars().next_back());
            let mut starts = Vec::new();
            let mut from = 0;
            while let Some(found) = self.text.get(from..).and_then(|rest| rest.find(term)) {
                let start = from + found;
                // `find` returns a char boundary, and so is the end of what it found.
                let before = self.text.get(..start).unwrap().chars().next_back();
                let rest = self.text.get(start..).unwrap();
                let after = rest.get(term.len()..).unwrap().chars().next();
                let joined_before = check_before && is_word(before);
                let joined_after = check_after && is_word(after);
                if !(joined_before || joined_after) {
                    starts.push(start);
                }
                from = start + rest.chars().next().map_or(1, char::len_utf8);
            }
            starts
        }
    }

    /// A string that must not appear in the repository: folded as text is, non-empty, with
    /// where it came from for the report.
    #[derive(Debug, Clone)]
    pub(super) struct Term {
        folded: String,
        source: Source,
    }

    impl Term {
        pub(super) fn parse(text: &str, source: Source) -> Option<Self> {
            let folded = Folded::of(text).text.trim().to_owned();
            (!folded.is_empty()).then_some(Self { folded, source })
        }

        pub(super) fn folded(&self) -> &str {
            &self.folded
        }

        pub(super) fn source(&self) -> Source {
            self.source
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) enum Source {
        Denylist,
        HomeDir,
        GitIdentity,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) enum Place {
        Path,
        Line(NonZeroUsize),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    Term(Source),
    Email,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    path: String,
    place: Place,
    rule: Rule,
    text: String,
}

/// RFC 2606: second-level domains reserved for documentation, and the top-level ones no real
/// address can be under.
const RESERVED_DOMAINS: [&str; 3] = ["example.com", "example.org", "example.net"];
const RESERVED_TLDS: [&str; 4] = ["test", "example", "invalid", "localhost"];

/// Whether `domain` (folded) is, or lies under, a domain reserved for examples.
fn is_reserved(domain: &str) -> bool {
    let under = |reserved: &str| {
        domain == reserved
            || domain
                .strip_suffix(reserved)
                .is_some_and(|head| head.ends_with('.'))
    };
    RESERVED_DOMAINS.iter().any(|reserved| under(reserved))
        || RESERVED_TLDS.iter().any(|tld| under(tld))
}

/// An address with a local part of letters, digits and `._%+-` or a quoted one, and a domain of
/// at least two labels, in any script.
fn email_pattern() -> Regex {
    Regex::new(
        r#"(?:[\p{Alphabetic}\p{N}._%+-]+|"[^"]+")@(?P<domain>(?:[\p{Alphabetic}\p{N}-]+\.)+\p{Alphabetic}{2,})"#,
    )
    .unwrap()
}

/// What a folded text is: a file's path, or the file's contents.
#[derive(Clone, Copy)]
enum Scanned {
    Path,
    Contents,
}

/// Every hit in `folded`, which is `path` itself or its contents.
fn hits_in(
    path: &str,
    folded: &Folded,
    scanned: Scanned,
    terms: &[Term],
    email: &Regex,
) -> Vec<Hit> {
    let place = |offset: usize| match scanned {
        Scanned::Path => Place::Path,
        Scanned::Contents => folded.line_at(offset),
    };
    let term_hits = terms.iter().flat_map(|term| {
        folded.word_starts(term).into_iter().map(|start| Hit {
            path: path.to_owned(),
            place: place(start),
            rule: Rule::Term(term.source()),
            text: term.folded().to_owned(),
        })
    });
    let email_hits = email
        .captures_iter(folded.text())
        .filter(|found| !is_reserved(&found["domain"]))
        .map(|found| {
            let whole = found.get(0).unwrap();
            Hit {
                path: path.to_owned(),
                place: place(whole.start()),
                rule: Rule::Email,
                text: whole.as_str().to_owned(),
            }
        });
    term_hits.chain(email_hits).collect()
}

/// Every hit in `path` itself and in `text`.
fn scan(path: &str, text: &str, terms: &[Term], email: &Regex) -> BTreeSet<Hit> {
    hits_in(path, &Folded::of(path), Scanned::Path, terms, email)
        .into_iter()
        .chain(hits_in(
            path,
            &Folded::of(text),
            Scanned::Contents,
            terms,
            email,
        ))
        .collect()
}

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("could not run git {args:?}: {error}"))
}

fn git_ok(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = git(root, args);
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    output.stdout
}

fn nul_separated(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            String::from_utf8(field.to_vec())
                .unwrap_or_else(|_| panic!("a path git listed is not UTF-8: {field:?}"))
        })
        .collect()
}

/// One line of `git ls-files --stage`: a file or a link has a blob in this repository; a
/// submodule's commit id is another repository's, so only its path is here to scan.
enum Staged {
    Object { id: String, path: String },
    Submodule { path: String },
}

impl Staged {
    fn parse(entry: &str) -> Self {
        let (meta, path) = entry.split_once('\t').unwrap();
        let fields: Vec<&str> = meta.split(' ').collect();
        let (mode, id) = (fields[0], fields[1]);
        let path = path.to_owned();
        match mode {
            "100644" | "100755" | "120000" => Self::Object {
                id: id.to_owned(),
                path,
            },
            "160000" => Self::Submodule { path },
            other => panic!("unknown mode {other} for {path}"),
        }
    }
}

/// The contents `git cat-file --batch` printed: for each object, `<id> <type> <size>\n`, the
/// object's bytes, then `\n`.
fn parse_batch(mut out: &[u8]) -> Vec<Vec<u8>> {
    let mut objects = Vec::new();
    while !out.is_empty() {
        let newline = out.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&out[..newline]).unwrap();
        let fields: Vec<&str> = header.split(' ').collect();
        assert_eq!(
            fields.len(),
            3,
            "cat-file could not give an object: {header}"
        );
        let size: usize = fields[2].parse().unwrap();
        let body = newline + 1;
        objects.push(out[body..body + size].to_vec());
        out = &out[body + size + 1..];
    }
    objects
}

/// The bytes of each object `ids` names, in order.
fn cat_file_batch(root: &Path, ids: &[&str]) -> Vec<Vec<u8>> {
    let input: String = ids.iter().flat_map(|id| [*id, "\n"]).collect();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("could not run git cat-file: {error}"));
    let mut stdin = child.stdin.take().unwrap();
    // Written from its own thread: git blocks on a full stdout pipe while we would block here.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    writer.join().unwrap().unwrap();
    assert!(
        child.wait().unwrap().success(),
        "git cat-file --batch failed"
    );
    let objects = parse_batch(&out);
    assert_eq!(objects.len(), ids.len(), "cat-file gave a different count");
    objects
}

/// Every staged path with its staged bytes (a link's target text; nothing for a submodule).
fn staged(root: &Path) -> Vec<(String, Vec<u8>)> {
    let entries: Vec<Staged> = nul_separated(&git_ok(root, &["ls-files", "-z", "--stage"]))
        .iter()
        .map(|entry| Staged::parse(entry))
        .collect();
    let (objects, submodules): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| matches!(entry, Staged::Object { .. }));
    let (ids, paths): (Vec<&str>, Vec<String>) = objects
        .iter()
        .filter_map(|entry| match entry {
            Staged::Object { id, path } => Some((id.as_str(), path.clone())),
            Staged::Submodule { .. } => None,
        })
        .unzip();
    let submodule_paths = submodules.into_iter().filter_map(|entry| match entry {
        Staged::Submodule { path } => Some((path, Vec::new())),
        Staged::Object { .. } => None,
    });
    paths
        .into_iter()
        .zip(cat_file_batch(root, &ids))
        .chain(submodule_paths)
        .collect()
}

/// Every committable path as it is in the working tree: a file's bytes, a symbolic link's
/// target text. A path gone from the working tree has nothing here; its staged copy, if any,
/// is read by [`staged`]. A directory is a submodule, read there by its path.
fn working_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let paths = nul_separated(&git_ok(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    ));
    paths
        .into_iter()
        .filter_map(|path| {
            let full = root.join(&path);
            match std::fs::symlink_metadata(&full) {
                Ok(meta) if meta.is_symlink() => {
                    let target = std::fs::read_link(&full).unwrap();
                    Some((path, target.as_os_str().as_encoded_bytes().to_vec()))
                }
                Ok(meta) if meta.is_dir() => None,
                Ok(_) => Some((path, std::fs::read(&full).unwrap())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("could not read {path}: {error}"),
            }
        })
        .collect()
}

fn scan_repository(root: &Path, terms: &[Term]) -> BTreeSet<Hit> {
    let email = email_pattern();
    staged(root)
        .into_iter()
        .chain(working_tree(root))
        .flat_map(|(path, bytes)| scan(&path, &String::from_utf8_lossy(&bytes), terms, &email))
        .collect()
}

/// The email in a `git var GIT_*_IDENT` line, `Name <email> time zone`.
fn ident_email(ident: &str) -> Option<&str> {
    Some(ident.split_once('<')?.1.split_once('>')?.0)
}

/// The emails `git var` says this repository's commits are authored and committed with,
/// environment overrides included. A failure means git has no identity, so no commit can be
/// made and none can leak; an identity it prints but this cannot read fails the test.
fn git_identity(root: &Path) -> Vec<Term> {
    ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"]
        .into_iter()
        .filter_map(|var| {
            let output = git(root, &["var", var]);
            output.status.success().then_some(output.stdout)
        })
        .filter_map(|ident| {
            let ident = String::from_utf8(ident).unwrap();
            let email = ident_email(&ident)
                .unwrap_or_else(|| panic!("git var printed no email: {ident:?}"));
            Term::parse(email, Source::GitIdentity)
        })
        .collect()
}

/// The home directory's path, and the same path as Claude Code names a project directory (every
/// character but a letter or digit a `-`), when it has at least two components: `/` and `/root`
/// name no one.
fn home_terms(home: &str) -> Vec<Term> {
    let home = home.trim_end_matches('/');
    if home.split('/').filter(|part| !part.is_empty()).count() < 2 {
        return Vec::new();
    }
    let dashed: String = home
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    [home, dashed.as_str()]
        .into_iter()
        .filter_map(|form| Term::parse(form, Source::HomeDir))
        .collect()
}

/// `xdg` when it is an absolute path (the XDG spec ignores an empty or relative one), else
/// `home/.config`; then `conn/leak-denylist` under it.
fn denylist_path(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let config = xdg
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            let home = home.unwrap_or_else(|| panic!("HOME is not set"));
            Path::new(&home).join(".config")
        });
    config.join("conn").join("leak-denylist")
}

/// The denylist file's terms: each line's text; a line starting with `#` is a comment, blank
/// lines carry none, and a leading byte-order mark is not part of the first term.
fn parse_denylist(text: &str) -> Vec<Term> {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| Term::parse(line, Source::Denylist))
        .collect()
}

enum Denylist {
    Read(Vec<Term>),
    Missing,
}

fn read_denylist(path: &Path) -> Denylist {
    match std::fs::read_to_string(path) {
        Ok(text) => Denylist::Read(parse_denylist(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Denylist::Missing,
        Err(error) => panic!("could not read the denylist {}: {error}", path.display()),
    }
}

#[test]
fn the_repository_holds_nothing_personal_company_specific_or_local() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = std::env::var("HOME").unwrap_or_else(|_| panic!("HOME is not set"));
    let path = denylist_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    );
    let denylist = match read_denylist(&path) {
        Denylist::Read(terms) => terms,
        Denylist::Missing => panic!(
            "no denylist at {}: list the names that must never be committed there (employer, \
             hosts, projects, channels), one per line; create it empty if there are none",
            path.display()
        ),
    };
    let terms: Vec<Term> = home_terms(&home)
        .into_iter()
        .chain(git_identity(&root))
        .chain(denylist)
        .collect();
    let report: Vec<String> = scan_repository(&root, &terms)
        .iter()
        .map(|hit| {
            format!(
                "{}: {:?}: {:?}: {}",
                hit.path, hit.place, hit.rule, hit.text
            )
        })
        .collect();
    assert!(
        report.is_empty(),
        "identifying text:\n{}",
        report.join("\n")
    );
}

// The tests below use synthetic text, assembled with `concat!` where the scan of this file
// would otherwise find it.

fn term(text: &str) -> Term {
    Term::parse(text, Source::Denylist).unwrap()
}

fn line(n: usize) -> Place {
    Place::Line(std::num::NonZeroUsize::new(n).unwrap())
}

fn places(hits: &BTreeSet<Hit>) -> Vec<Place> {
    hits.iter().map(|hit| hit.place).collect()
}

fn folded_terms(terms: &[Term]) -> Vec<&str> {
    terms.iter().map(Term::folded).collect()
}

/// Mutation, each ran red: match case-sensitively; skip the path; count no lines; match inside
/// a word.
#[test]
fn a_term_is_found_as_a_word_in_any_case_in_a_line_and_in_the_path() {
    let hits = scan(
        "src/acme.rs",
        "first\nbuilt for ACME corp\nacmes and macme are other words\n",
        &[term("Acme")],
        &email_pattern(),
    );
    assert_eq!(places(&hits), [Place::Path, line(2)]);
}

/// Mutation, each ran red: fold no whitespace; keep each whitespace character; report a later
/// byte's line.
#[test]
fn a_term_split_by_any_whitespace_is_found_on_the_line_it_starts() {
    let terms = [term("jane  doe")];
    let text = "a\nby Jane\n  Doe\nJane\u{a0}Doe\njane\tdoe\n";
    let hits = scan("x", text, &terms, &email_pattern());
    assert_eq!(places(&hits), [line(2), line(4), line(5)]);
}

/// Mutation, ran red: resume the search after a rejected match's end.
#[test]
fn a_rejected_match_does_not_hide_an_overlapping_one() {
    let hits = scan("x", "xa-a-a\n", &[term("a-a")], &email_pattern());
    assert_eq!(places(&hits), [line(1)]);
}

/// Mutation, ran red: check the edge after a term that ends in `-`.
#[test]
fn a_key_prefix_term_matches_the_keys_it_starts() {
    let hits = scan(
        "x",
        "see ABC-123\nxabc-1 is not it\n",
        &[term("abc-")],
        &email_pattern(),
    );
    assert_eq!(places(&hits), [line(1)]);
}

/// Mutation, each ran red: allow every domain; accept a reserved name as a bare suffix (no dot
/// before it); accept a reserved TLD as any label; take ASCII addresses only; drop the quoted
/// local part; skip the path.
#[test]
fn an_email_outside_the_reserved_domains_is_a_hit() {
    let leaked = [
        concat!("someone", "@", "acme.io"),
        concat!("x", "@", "notexample.com"),
        concat!("y", "@", "test.acme.io"),
        concat!("josé", "@", "acmé.io"),
        concat!("\"jane doe\"", "@", "acme.io"),
    ];
    let reserved = [
        concat!("ada", "@", "EXAMPLE.com"),
        concat!("x", "@", "example.org"),
        concat!("a", "@", "mail.example.test"),
        concat!("b", "@", "evil.example"),
    ];
    let text = leaked
        .iter()
        .chain(&reserved)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let hits = scan("README.md", &text, &[], &email_pattern());
    let mut found: Vec<&str> = hits.iter().map(|hit| hit.text.as_str()).collect();
    found.sort_unstable();
    let mut expected = leaked.to_vec();
    expected.sort_unstable();
    assert_eq!(found, expected);
    let in_path = scan(
        concat!("docs/me", "@", "acme.io.md"),
        "",
        &[],
        &email_pattern(),
    );
    assert_eq!(places(&in_path), [Place::Path]);
}

/// Mutation, each ran red: treat a `#` anywhere as a comment; keep blank lines as empty terms;
/// keep the byte-order mark.
#[test]
fn a_denylist_line_is_a_term_unless_it_starts_with_a_hash() {
    let terms = parse_denylist("\u{feff}acme\n# employers\n  #team-secret\n\n   \nC# shop\n");
    assert_eq!(folded_terms(&terms), ["acme", "c# shop"]);
    let hits = scan("x", "acme\n", &terms, &email_pattern());
    assert_eq!(places(&hits), [line(1)]);
}

/// Mutation, each ran red: read a missing file as an empty denylist; read an empty one as
/// missing.
#[test]
fn a_missing_denylist_is_told_apart_from_an_empty_one() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no_leaks_denylist");
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty");
    std::fs::write(&empty, "").unwrap();
    assert!(matches!(
        read_denylist(&dir.join("absent")),
        Denylist::Missing
    ));
    assert!(matches!(read_denylist(&empty), Denylist::Read(terms) if terms.is_empty()));
}

/// Mutation, each ran red: use an empty `XDG_CONFIG_HOME`; use a relative one.
#[test]
fn the_denylist_lives_under_an_absolute_xdg_config_home_else_home() {
    let at = |xdg: Option<&str>| denylist_path(xdg.map(OsString::from), Some("/h/u".into()));
    assert_eq!(at(Some("/cfg")), Path::new("/cfg/conn/leak-denylist"));
    assert_eq!(at(Some("")), Path::new("/h/u/.config/conn/leak-denylist"));
    assert_eq!(
        at(Some("rel")),
        Path::new("/h/u/.config/conn/leak-denylist")
    );
    assert_eq!(at(None), Path::new("/h/u/.config/conn/leak-denylist"));
}

/// Mutation, each ran red: drop the dashed form; keep a one-component home; keep the trailing
/// slash.
#[test]
fn a_home_is_searched_as_a_path_and_as_a_project_directory_name() {
    assert_eq!(
        folded_terms(&home_terms("/home/JDoe/")),
        ["/home/jdoe", "-home-jdoe"]
    );
    assert!(home_terms("/").is_empty());
    assert!(home_terms("/root").is_empty());
    let hits = scan(
        "tests/fixtures/p/-home-jdoe-work-x/a.jsonl",
        "cwd /home/jdoe/work\n",
        &home_terms("/home/jdoe"),
        &email_pattern(),
    );
    assert_eq!(places(&hits), [Place::Path, line(1)]);
}

/// Mutation, ran red: take the text after `>`.
#[test]
fn an_identity_line_gives_its_email() {
    let ident = concat!("Jo Doe <jo", "@", "acme.io> 1761645600 +0000");
    assert_eq!(ident_email(ident), Some(concat!("jo", "@", "acme.io")));
    assert_eq!(ident_email("no address"), None);
}

/// Mutation, ran red: take the size field as the second one.
#[test]
fn a_batch_reads_as_each_objects_bytes() {
    let out = b"aaa blob 3\nab\n\nbbb blob 0\n\n";
    assert_eq!(parse_batch(out), [b"ab\n".to_vec(), Vec::new()]);
}

/// What `git commit` takes is the staged blob, and a link is committed as its target's text:
/// a leak staged and then removed from the working copy, a staged file deleted from the
/// working tree, a staged link and an untracked link pointing at a leaking path (to a clean
/// file) are all found, and a submodule is read by its path.
/// Mutation, each ran red: scan the working tree alone; follow an untracked link to its file;
/// ask cat-file for a submodule's commit.
#[test]
fn the_staged_copy_and_a_links_text_are_what_is_scanned() {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no_leaks");
    let _ = std::fs::remove_dir_all(&scratch);
    let root = scratch.join("repo");
    let target = scratch.join("acme").join("notes.md");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "nothing to see\n").unwrap();
    git_ok(&root, &["init", "-q"]);
    std::fs::write(root.join("notes.md"), "built for Acme\n").unwrap();
    std::fs::write(root.join("gone.md"), "Acme again\n").unwrap();
    std::os::unix::fs::symlink(&target, root.join("link.md")).unwrap();
    git_ok(&root, &["add", "-A"]);
    git_ok(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,acme-lib",
        ],
    );
    std::fs::write(root.join("notes.md"), "built for someone\n").unwrap();
    std::fs::remove_file(root.join("gone.md")).unwrap();
    std::os::unix::fs::symlink(&target, root.join("later.md")).unwrap();
    let hits = scan_repository(&root, &[term("acme")]);
    let found: Vec<(&str, Place)> = hits
        .iter()
        .map(|hit| (hit.path.as_str(), hit.place))
        .collect();
    assert_eq!(
        found,
        [
            ("acme-lib", Place::Path),
            ("gone.md", line(1)),
            ("later.md", line(1)),
            ("link.md", line(1)),
            ("notes.md", line(1))
        ]
    );
}
