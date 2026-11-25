//! Hand-written tables: an input, and the links expected with the exact text each one
//! spans. Spans are written as the matched text, so a reader checks them against the
//! input by eye; the test turns each into a byte range with `str::find`, which does not
//! use the code under test. Placeholder hosts and prefixes only.

use proptest::prelude::*;

use super::*;
use std::path::PathBuf;

use crate::types::{FileRoots, Host, JiraKey, LocalFile, SafeUrl};

const JIRA_HOST: &str = "example.atlassian.net";
const SLACK_HOST: &str = "example.slack.com";

fn extractor_with_owner(owner: Option<&str>) -> Extractor {
    let targets = LinkTargets {
        jira: Some(Host::parse(JIRA_HOST).unwrap()),
        slack: Some(Host::parse(SLACK_HOST).unwrap()),
    };
    let prefixes = [
        JiraPrefix::parse("EX").unwrap(),
        JiraPrefix::parse("PROJ").unwrap(),
    ];
    Extractor::new(&targets, &prefixes, owner, roots()).unwrap()
}

fn roots() -> FileRoots {
    FileRoots::new(vec![
        PathBuf::from("/home/dev"),
        PathBuf::from("/run/scratch"),
    ])
    .unwrap()
}

fn extractor() -> Extractor {
    extractor_with_owner(Some("example-org"))
}

fn jira(prefix: &str, number: u32) -> Link {
    Link::Jira {
        key: JiraKey::new(
            JiraPrefix::parse(prefix).unwrap(),
            std::num::NonZeroU32::new(number).unwrap(),
        ),
    }
}

fn pull(owner: &str, repo: &str, number: u32) -> Link {
    Link::PullRequest {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
    }
}

fn slack(channel: &str, ts: &str) -> Link {
    Link::SlackThread {
        channel: channel.to_owned(),
        ts: ts.to_owned(),
    }
}

fn doc(url: &str) -> Link {
    Link::Doc {
        url: SafeUrl::parse(url).unwrap(),
    }
}

fn other(url: &str) -> Link {
    Link::Other {
        url: SafeUrl::parse(url).unwrap(),
    }
}

fn in_repo() -> LinkContext {
    LinkContext {
        repo: Some(RepoRef {
            owner: "example-org".to_owned(),
            repo: "example-repo".to_owned(),
        }),
    }
}

/// The expected occurrence: the link and the text it spans, found by its first appearance
/// in the input at or after `from`.
fn expect(input: &str, needle: &str, link: Link) -> Found {
    let start = input.find(needle).unwrap();
    Found {
        link,
        span: start..start + needle.len(),
    }
}

fn check(input: &str, ctx: &LinkContext, expected: Vec<Found>, dropped: u32) {
    let got = extractor().extract(input, ctx);
    assert_eq!(
        got,
        Extraction {
            found: expected,
            dropped
        },
        "input: {input:?}"
    );
}

#[test]
fn jira_keys_by_configured_prefix() {
    let input = "see EX-12 and PROJ-3, not EXX-1";
    check(
        input,
        &LinkContext::default(),
        vec![
            expect(input, "EX-12", jira("EX", 12)),
            expect(input, "PROJ-3", jira("PROJ", 3)),
        ],
        0,
    );
}

/// Bare numbers continuing a key's list are keys of its prefix, each linked on its digits.
/// Mutation: drop the `key_list_tail` call (only the two keys are found), or the
/// `and|or` alternative (the list stops at `134`).
#[test]
fn a_list_after_a_key_continues_it() {
    let input = "PROJ-123, 134 and 345; EX-581/694 & 695, or 696.";
    check(
        input,
        &LinkContext::default(),
        vec![
            expect(input, "PROJ-123", jira("PROJ", 123)),
            expect(input, "134", jira("PROJ", 134)),
            expect(input, "345", jira("PROJ", 345)),
            expect(input, "EX-581", jira("EX", 581)),
            expect(input, "694", jira("EX", 694)),
            expect(input, "695", jira("EX", 695)),
            expect(input, "696", jira("EX", 696)),
        ],
        0,
    );
}

/// A date, a count, a version, a `#` reference, a word or a line break after a key
/// ends its list, and so does the first number that cannot continue it.
/// Mutation: drop the digit-count check (`2026`, `45` and `678` become keys), or the
/// following-character check (`2026` and `456abc`).
#[test]
fn a_number_that_is_not_a_key_ends_the_list() {
    for input in [
        "PROJ-906, 2026-03-14",
        "PROJ-123, 2 files",
        "PROJ-123, 1.2",
        "PROJ-123, 456abc",
        "PROJ-314, #2718",
        "PROJ-123,\n456",
        "PROJ-123, 45, 678",
    ] {
        let key = input.split(',').next().unwrap();
        let number = key.trim_start_matches("PROJ-").parse().unwrap();
        check(
            input,
            &LinkContext::default(),
            vec![expect(input, key, jira("PROJ", number))],
            0,
        );
    }
}

#[test]
fn the_prefix_list_is_the_whole_filter() {
    for input in [
        "CVE-2026-1 SHA-256 UTF-8 ISO-8601 ABC-123",
        "XEX-1 EXPROJ-2 EX-1x EX-1_a ex-5 Ex-5 EX- EX-0 EX-99999999999",
        "EX 12, EX–12",
    ] {
        check(input, &LinkContext::default(), vec![], 0);
    }
}

#[test]
fn a_prefix_in_the_list_is_matched_even_if_it_looks_generic() {
    // Whatever the configuration names is a key; the extractor adds no judgement.
    let targets = LinkTargets {
        jira: Some(Host::parse(JIRA_HOST).unwrap()),
        slack: Some(Host::parse(SLACK_HOST).unwrap()),
    };
    let sha = Extractor::new(
        &targets,
        &[JiraPrefix::parse("SHA").unwrap()],
        None,
        FileRoots::default(),
    )
    .unwrap();
    let input = "SHA-256 and CVE-1";
    let got = sha.extract(input, &LinkContext::default());
    assert_eq!(got.found, vec![expect(input, "SHA-256", jira("SHA", 256))]);
}

/// Without Jira nothing is a key: not a prefixed key, not a bare `-123`, and a Jira URL is
/// just a URL, not a document. Building the key pattern from the empty prefix list is not
/// caught here and need not be: `jira_link` admits only a configured prefix, so the empty
/// one matches nothing (ran, stayed green).
#[test]
fn without_jira_nothing_is_a_key() {
    let targets = LinkTargets {
        jira: None,
        slack: Some(Host::parse(SLACK_HOST).unwrap()),
    };
    let none = Extractor::new(&targets, &[], None, FileRoots::default()).unwrap();
    let browse = "https://example.atlassian.net/browse/EX-1";
    let wiki = "https://example.atlassian.net/wiki/spaces/X";
    let input = format!("EX-12, x-123 and -7 at {browse} and {wiki}");
    assert_eq!(
        none.extract(&input, &LinkContext::default()).found,
        vec![
            expect(&input, browse, other(browse)),
            expect(&input, wiki, other(wiki)),
        ]
    );
}

/// Without Slack a permalink is not a thread and a Slack document is not a document: both
/// are just URLs.
/// Mutation: build the permalink pattern against a placeholder host when none is configured
/// (the permalink becomes a thread), or keep the Slack docs prefix (the docs URL is a doc).
#[test]
fn without_slack_a_slack_url_is_just_a_url() {
    let targets = LinkTargets {
        jira: Some(Host::parse(JIRA_HOST).unwrap()),
        slack: None,
    };
    let none = Extractor::new(
        &targets,
        &[JiraPrefix::parse("EX").unwrap()],
        None,
        FileRoots::default(),
    )
    .unwrap();
    let permalink = "https://example.slack.com/archives/C0123ABCD/p1788350000000100";
    let docs = "https://example.slack.com/docs/T0123ABCD/F0123ABCD";
    let input = format!("{permalink} and {docs}");
    assert_eq!(
        none.extract(&input, &LinkContext::default()).found,
        vec![
            expect(&input, permalink, other(permalink)),
            expect(&input, docs, other(docs)),
        ]
    );
}

#[test]
fn a_key_is_found_next_to_punctuation_and_unicode() {
    let input = "é(EX-1), «PROJ-2» é";
    check(
        input,
        &LinkContext::default(),
        vec![
            expect(input, "EX-1", jira("EX", 1)),
            expect(input, "PROJ-2", jira("PROJ", 2)),
        ],
        0,
    );
}

#[test]
fn a_browse_url_is_one_link_not_two_and_its_query_is_part_of_it() {
    let input = "x https://example.atlassian.net/browse/EX-7?focusedCommentId=5#comment y";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            "https://example.atlassian.net/browse/EX-7?focusedCommentId=5#comment",
            jira("EX", 7),
        )],
        0,
    );
}

#[test]
fn a_browse_url_of_an_unconfigured_prefix_is_other() {
    let input = "https://example.atlassian.net/browse/ZZ-7";
    check(
        input,
        &LinkContext::default(),
        vec![expect(input, input, other(input))],
        0,
    );
}

#[test]
fn a_key_inside_some_other_url_does_not_make_a_second_link() {
    let input = "https://example.com/tickets/EX-9";
    check(
        input,
        &LinkContext::default(),
        vec![expect(input, input, other(input))],
        0,
    );
}

#[test]
fn of_two_readings_of_the_same_start_the_more_specific_wins() {
    // `EX-12#5` is a key, and also a repository called `EX-12` followed by a number.
    let input = "EX-12#5";
    check(
        input,
        &LinkContext::default(),
        vec![expect(input, "EX-12", jira("EX", 12))],
        0,
    );
}

#[test]
fn links_are_deduplicated_in_order_of_first_appearance() {
    let input = "EX-1 PROJ-2 EX-1 https://example.atlassian.net/browse/PROJ-2 EX-3";
    check(
        input,
        &LinkContext::default(),
        vec![
            expect(input, "EX-1", jira("EX", 1)),
            expect(input, "PROJ-2", jira("PROJ", 2)),
            expect(input, "EX-3", jira("EX", 3)),
        ],
        0,
    );
}

#[test]
fn slack_permalink_becomes_a_dotted_timestamp() {
    let input = "https://example.slack.com/archives/C0123ABCD/p1788350000000100";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            input,
            slack("C0123ABCD", "1788350000.000100"),
        )],
        0,
    );
}

#[test]
fn a_reply_permalink_names_the_parent_thread() {
    let input = "https://example.slack.com/archives/C0123ABCD/p1788352999000001?thread_ts=1788350000.000100&cid=C0123ABCD";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            input,
            slack("C0123ABCD", "1788350000.000100"),
        )],
        0,
    );
}

#[test]
fn a_malformed_thread_ts_falls_back_to_the_permalink_timestamp() {
    let input = "https://example.slack.com/archives/C0123ABCD/p1788352999000001?thread_ts=oops";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            input,
            slack("C0123ABCD", "1788352999.000001"),
        )],
        0,
    );
}

#[test]
fn a_slack_channel_link_is_other_not_a_thread() {
    for input in [
        "https://app.slack.com/client/T0123ABCD/C0123ABCD",
        "https://example.slack.com/archives/C0123ABCD",
        "https://example.slack.com/archives/C0123ABCD/p12345",
    ] {
        check(
            input,
            &LinkContext::default(),
            vec![expect(input, input, other(input))],
            0,
        );
    }
}

#[test]
fn pull_request_urls_ignore_tails() {
    for tail in [
        "",
        "/files",
        "/changes",
        "#discussion_r1",
        "?diff=split",
        "/files?w=1#x",
    ] {
        let url = format!("https://github.com/example-org/example-repo/pull/42{tail}");
        check(
            &url,
            &LinkContext::default(),
            vec![expect(&url, &url, pull("example-org", "example-repo", 42))],
            0,
        );
    }
}

#[test]
fn a_pull_url_with_a_zero_number_or_an_issue_url_is_other() {
    for url in [
        "https://github.com/example-org/example-repo/pull/0",
        "https://github.com/example-org/example-repo/issues/42",
        "https://github.com/example-org/example-repo/pull/42x",
    ] {
        check(
            url,
            &LinkContext::default(),
            vec![expect(url, url, other(url))],
            0,
        );
    }
}

#[test]
fn owner_slash_repo_hash_number() {
    let input = "(example-org/example-repo#42) and other-org/other.repo#7.";
    check(
        input,
        &LinkContext::default(),
        vec![
            expect(
                input,
                "example-org/example-repo#42",
                pull("example-org", "example-repo", 42),
            ),
            expect(
                input,
                "other-org/other.repo#7",
                pull("other-org", "other.repo", 7),
            ),
        ],
        0,
    );
}

#[test]
fn pull_request_number_zero_is_not_a_pull_request() {
    let input = "example-org/example-repo#0 example-repo#0 #00";
    let got = extractor().extract(input, &in_repo());
    assert_eq!(got.found, vec![]);
}

#[test]
fn a_reference_in_the_middle_of_a_path_is_not_a_pull_request() {
    let input = "see src/example-org/example-repo#42";
    check(input, &LinkContext::default(), vec![], 0);
}

#[test]
fn bare_numbers_need_a_repository_and_a_boundary() {
    let input = "fix #123 (#45) #6 #1234567 a#99 x#123 #123abc\n#78";
    let expected = vec![
        expect(input, "#123", pull("example-org", "example-repo", 123)),
        expect(input, "#45", pull("example-org", "example-repo", 45)),
        expect(input, "#78", pull("example-org", "example-repo", 78)),
    ];
    let with_repo = extractor_with_owner(None).extract(input, &in_repo());
    assert_eq!(with_repo.found, expected);
    let without_repo = extractor_with_owner(None).extract(input, &LinkContext::default());
    assert_eq!(without_repo.found, vec![]);
}

#[test]
fn repo_hash_number_resolves_against_the_default_owner_only() {
    let input = "see example-repo#12 now";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            "example-repo#12",
            pull("example-org", "example-repo", 12),
        )],
        0,
    );
    let no_owner = extractor_with_owner(None).extract(input, &LinkContext::default());
    assert_eq!(no_owner.found, vec![]);
}

#[test]
fn a_default_owner_that_is_not_an_owner_name_is_refused() {
    let targets = LinkTargets {
        jira: Some(Host::parse(JIRA_HOST).unwrap()),
        slack: Some(Host::parse(SLACK_HOST).unwrap()),
    };
    let result = Extractor::new(
        &targets,
        &[JiraPrefix::parse("EX").unwrap()],
        Some("a b"),
        FileRoots::default(),
    );
    assert!(matches!(result, Err(LinksError::InvalidOwner(_))));
}

#[test]
fn documents() {
    for url in [
        "https://docs.google.com/document/d/abc123/edit",
        "https://claude.ai/artifact/abc-123",
        "https://example.atlassian.net/wiki/spaces/EX/pages/123/Title",
        "https://example.slack.com/docs/T0123ABCD/F0123ABCD",
    ] {
        check(
            url,
            &LinkContext::default(),
            vec![expect(url, url, doc(url))],
            0,
        );
    }
}

#[test]
fn other_https_urls_lose_trailing_punctuation_only() {
    let cases = [
        ("see https://example.com/a.", "https://example.com/a"),
        ("(https://example.com/a)", "https://example.com/a"),
        ("[https://example.com/a]", "https://example.com/a"),
        ("<https://example.com/a>", "https://example.com/a"),
        ("'https://example.com/a'", "https://example.com/a"),
        ("\"https://example.com/a\"", "https://example.com/a"),
        ("https://example.com/a,", "https://example.com/a"),
        ("https://example.com/a;", "https://example.com/a"),
        ("https://example.com/a:", "https://example.com/a"),
        ("https://example.com/a).", "https://example.com/a"),
        ("https://example.com/a?b=c.d", "https://example.com/a?b=c.d"),
        (
            "https://en.example.org/wiki/Foo_(bar)",
            "https://en.example.org/wiki/Foo_(bar)",
        ),
        (
            "(https://en.example.org/wiki/Foo_(bar))",
            "https://en.example.org/wiki/Foo_(bar)",
        ),
        ("https://example.com/a.b.", "https://example.com/a.b"),
    ];
    for (input, url) in cases {
        check(
            input,
            &LinkContext::default(),
            vec![expect(input, url, other(url))],
            0,
        );
    }
}

#[test]
fn http_and_unsafe_urls_are_dropped_and_counted() {
    let input = "http://example.com/a https://user@example.com/b https://example.com/ok";
    check(
        input,
        &LinkContext::default(),
        vec![expect(
            input,
            "https://example.com/ok",
            other("https://example.com/ok"),
        )],
        2,
    );
}

/// `http://` to this machine is a link and is not counted as dropped.
/// Mutation: refuse local hosts in `SafeUrl::parse` (the first two links vanish and the count
/// becomes 3), or narrow the URL token regex to `https://` (the same).
#[test]
fn http_to_a_local_host_is_a_link_and_other_http_is_dropped() {
    let input = "http://localhost:5173/x [dev](http://127.0.0.1:8082/g) http://example.com/a";
    let result = extractor().extract(input, &LinkContext::default());
    assert_eq!(
        result.found,
        vec![
            expect(
                input,
                "http://localhost:5173/x",
                other("http://localhost:5173/x")
            ),
            expect(
                input,
                "[dev](http://127.0.0.1:8082/g)",
                other("http://127.0.0.1:8082/g")
            ),
        ]
    );
    assert_eq!(result.dropped, 1);
}

#[test]
fn a_key_inside_a_dropped_http_url_is_still_a_key_and_the_drop_is_still_counted() {
    let input = "http://example.atlassian.net/browse/EX-1";
    check(
        input,
        &LinkContext::default(),
        vec![expect(input, "EX-1", jira("EX", 1))],
        1,
    );
}

#[test]
fn markdown_links_yield_the_url_once_shown_as_the_label() {
    let input = "read [the spec](https://example.com/spec) and [EX-5](https://example.com/b).";
    let result = extractor().extract(input, &LinkContext::default());
    assert_eq!(
        result.found,
        vec![
            expect(
                input,
                "[the spec](https://example.com/spec)",
                other("https://example.com/spec")
            ),
            expect(
                input,
                "[EX-5](https://example.com/b)",
                other("https://example.com/b")
            ),
        ]
    );
    let segmented = extractor().segments(input, &LinkContext::default());
    assert_eq!(
        segmented.segments,
        vec![
            Segment::Text("read "),
            Segment::Link {
                link: other("https://example.com/spec"),
                shown: "the spec",
                source: "[the spec](https://example.com/spec)",
            },
            Segment::Text(" and "),
            Segment::Link {
                link: other("https://example.com/b"),
                shown: "EX-5",
                source: "[EX-5](https://example.com/b)",
            },
            Segment::Text("."),
        ]
    );
}

#[test]
fn an_http_url_inside_a_markdown_label_is_label_text_not_an_omission() {
    let input = "[see http://example.com/a](https://example.com/b)";
    check(
        input,
        &LinkContext::default(),
        vec![expect(input, input, other("https://example.com/b"))],
        0,
    );
}

#[test]
fn a_markdown_link_to_http_is_dropped_and_counted_once() {
    let input = "[x](http://example.com/a)";
    check(input, &LinkContext::default(), vec![], 1);
}

#[test]
fn segments_cover_the_text_and_repeat_every_occurrence() {
    let input = "é EX-1 and EX-1";
    let segmented = extractor().segments(input, &LinkContext::default());
    assert_eq!(
        segmented.segments,
        vec![
            Segment::Text("é "),
            Segment::Link {
                link: jira("EX", 1),
                shown: "EX-1",
                source: "EX-1",
            },
            Segment::Text(" and "),
            Segment::Link {
                link: jira("EX", 1),
                shown: "EX-1",
                source: "EX-1",
            },
        ]
    );
    assert_eq!(segmented.dropped, 0);
}

#[test]
fn no_links_is_one_text_segment_and_no_text_is_no_segments() {
    let none = extractor().segments("", &LinkContext::default());
    assert_eq!(none.segments, vec![]);
    let plain = extractor().segments("nothing here", &LinkContext::default());
    assert_eq!(plain.segments, vec![Segment::Text("nothing here")]);
}

/// The local files a text's segments open, by path.
fn files_in(input: &str) -> Vec<String> {
    extractor()
        .segments(input, &LinkContext::default())
        .segments
        .into_iter()
        .filter_map(|s| match s {
            Segment::File { file, source } => {
                assert_eq!(file.to_string(), source);
                Some(source.to_owned())
            }
            Segment::Text(_) | Segment::Link { .. } => None,
        })
        .collect()
}

/// A path under a root is a file segment wherever prose puts it; one outside the roots, not
/// absolute, not `.md` or with `..` is text. Files are not links, so `extract` leaves them
/// out. Mutation: drop the roots check in `local_file` (`/elsewhere` is found), or push files
/// into `extract` (the `found` assertion).
#[test]
fn a_markdown_path_under_a_root_is_a_file() {
    let input = "brief at /run/scratch/s/plan-brief.md, see (/home/dev/a/b.md) and \
        \"/home/dev/q.md\"; not /elsewhere/x.md, /home/dev/x.txt, /home/dev/../etc/p.md, \
        home/dev/r.md or /home/dev/v.mdx";
    assert_eq!(
        files_in(input),
        [
            "/run/scratch/s/plan-brief.md",
            "/home/dev/a/b.md",
            "/home/dev/q.md"
        ]
    );
    assert_eq!(
        extractor().extract(input, &LinkContext::default()).found,
        vec![]
    );
}

/// A path inside a URL is part of the URL.
#[test]
fn a_path_inside_a_url_is_not_a_file() {
    assert_eq!(
        files_in("https://example.com/home/dev/a.md"),
        Vec::<String>::new()
    );
}

#[test]
fn a_code_span_that_is_exactly_a_path_is_a_file() {
    let extractor = extractor();
    assert_eq!(
        extractor.local_file("/home/dev/a.md"),
        Some(LocalFile::parse("/home/dev/a.md").unwrap())
    );
    for not in [
        "/home/dev/a.md and more",
        "/elsewhere/a.md",
        " /home/dev/a.md",
    ] {
        assert_eq!(extractor.local_file(not), None, "{not:?}");
    }
}

fn source_of(segments: &[Segment<'_>]) -> String {
    segments
        .iter()
        .map(|s| match s {
            Segment::Text(t) => *t,
            Segment::Link { source, .. } | Segment::File { source, .. } => source,
        })
        .collect()
}

/// Strings built from the fragments the patterns care about, in any order, so the
/// interesting overlaps are common.
fn fragmentary() -> impl Strategy<Value = String> {
    let fragment = prop_oneof![
        Just("EX-12"),
        Just("PROJ-7"),
        Just("https://example.atlassian.net/browse/EX-1?x=1"),
        Just("https://example.slack.com/archives/C0123ABCD/p1788350000000100"),
        Just("[label](https://example.com/a)"),
        Just("[EX-1](http://example.com/a)"),
        Just("http://example.com/"),
        Just("https://user@example.com/"),
        Just("https://github.com/example-org/example-repo/pull/9/files"),
        Just("example-org/example-repo#42"),
        Just("example-repo#5"),
        Just("#123"),
        Just("/home/dev/a.md"),
        Just("/elsewhere/b.md"),
        Just("`"),
        Just("("),
        Just(")"),
        Just("]"),
        Just("."),
        Just(" "),
        Just("\n"),
        Just("é"),
        Just("日本"),
    ];
    prop::collection::vec(fragment, 0..12).prop_map(|parts| parts.concat())
}

proptest! {
    /// Methodology: whatever the input, the segments in order are the input, and the
    /// links found are the links segmented.
    #[test]
    fn segments_round_trip_arbitrary_text(text in ".*") {
        let segmented = extractor().segments(&text, &in_repo());
        prop_assert_eq!(source_of(&segmented.segments), text);
    }

    #[test]
    fn segments_round_trip_fragmentary_text(text in fragmentary()) {
        let segmented = extractor().segments(&text, &in_repo());
        prop_assert_eq!(source_of(&segmented.segments), text.clone());
        let in_segments: HashSet<Link> = segmented
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Link { link, .. } => Some(link.clone()),
                Segment::Text(_) | Segment::File { .. } => None,
            })
            .collect();
        let extracted: HashSet<Link> = extractor()
            .extract(&text, &in_repo())
            .found
            .into_iter()
            .map(|f| f.link)
            .collect();
        prop_assert_eq!(in_segments, extracted);
    }
}
