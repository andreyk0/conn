//! Assistant text as markup, without ever writing a string of HTML.
//!
//! [`render`] folds the event stream of a markdown parser into `maud` markup. Each container
//! (a paragraph, a list, a table) is a frame of children on an explicit stack, closed into its
//! element when its end event arrives, so a deeply nested input costs heap and not call depth.
//!
//! What third-party text can make appear is what this module draws and nothing else:
//!
//! - raw HTML, block or inline, is shown as the text it is;
//! - a link's `href` is a [`SafeUrl`] or the link is its text alone; an image is its alt text;
//! - text runs, once markdown has decoded entities and escapes, are redacted again and pass
//!   through the link extractor, so Jira keys, Slack permalinks and pull-request references
//!   become anchors too (code is left as written, except that a code span holding only the
//!   path of a local file the site shows opens that file);
//! - what the links refused are counted in [`Rendered::withheld`].

use maud::{Markup, html};
use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TextMergeStream};

use super::fmt::{external_link, file_link, redact};
use crate::links::{Extractor, LinkContext, Segment};
use crate::types::{LinkTargets, SafeUrl};
use crate::util::redact::Redacted;

/// Markup, and how many links were left out of it.
pub(super) struct Rendered {
    pub markup: Markup,
    /// Links the text carried that were refused (not `https://` or local `http://`, or not safe to follow) or
    /// could not be written as a URL; their text is shown without an anchor.
    pub withheld: u32,
}

/// The markdown extensions the text is read with.
const EXTENSIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

pub(super) fn render(
    extractor: &Extractor,
    targets: &LinkTargets,
    source: &Redacted<String>,
) -> Rendered {
    let mut fold = Fold {
        extractor,
        targets,
        root: Vec::new(),
        stack: Vec::new(),
        withheld: 0,
    };
    for event in TextMergeStream::new(Parser::new_ext(source.as_str(), EXTENSIONS)) {
        fold.event(event);
    }
    fold.finish()
}

/// The deepest heading a card shows: the card's own title is an `h2`, so the text's headings
/// start below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heading {
    Third,
    Fourth,
}

impl Heading {
    const fn of(level: HeadingLevel) -> Self {
        match level {
            HeadingLevel::H1 => Self::Third,
            HeadingLevel::H2
            | HeadingLevel::H3
            | HeadingLevel::H4
            | HeadingLevel::H5
            | HeadingLevel::H6 => Self::Fourth,
        }
    }
}

/// How a list counts its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Numbering {
    Bulleted,
    From(u64),
}

/// What an open frame will become.
enum Container {
    Paragraph,
    Heading(Heading),
    BlockQuote,
    CodeBlock,
    /// Raw HTML the text carried, shown as the text it is.
    RawBlock,
    List(Numbering),
    Item,
    Table(Vec<Alignment>),
    TableHead,
    TableRow,
    TableCell,
    Emphasis,
    Strong,
    Strikethrough,
    /// A link whose destination is safe; its children are inside the anchor.
    Anchor(SafeUrl),
    /// A link that is not followed, and an image: the children alone.
    Plain,
}

struct Frame {
    container: Container,
    children: Vec<Markup>,
}

struct Fold<'a> {
    extractor: &'a Extractor,
    targets: &'a LinkTargets,
    /// The finished top-level blocks.
    root: Vec<Markup>,
    /// The open containers, outermost first.
    stack: Vec<Frame>,
    withheld: u32,
}

impl Fold<'_> {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => {
                let container = self.open(tag);
                self.stack.push(Frame {
                    container,
                    children: Vec::new(),
                });
            }
            Event::End(_) => self.close(),
            Event::Text(text) => {
                let markup = self.text(&text);
                self.push(markup);
            }
            Event::Code(code) => {
                let markup = self.code(&redact(&code));
                self.push(markup);
            }
            Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::FootnoteReference(text) => {
                let text = redact(&text);
                self.push(html! { (text) });
            }
            Event::SoftBreak => self.push(html! { "\n" }),
            Event::HardBreak => self.push(html! { br; }),
            Event::Rule => self.push(html! { hr; }),
            Event::TaskListMarker(checked) => {
                self.push(html! { input type="checkbox" disabled checked[checked]; " " });
            }
        }
    }

    fn open(&mut self, tag: Tag<'_>) -> Container {
        match tag {
            Tag::Paragraph => Container::Paragraph,
            Tag::Heading { level, .. } => Container::Heading(Heading::of(level)),
            Tag::BlockQuote(_) => Container::BlockQuote,
            Tag::CodeBlock(_) => Container::CodeBlock,
            Tag::HtmlBlock => Container::RawBlock,
            Tag::List(first) => Container::List(first.map_or(Numbering::Bulleted, Numbering::From)),
            Tag::Item => Container::Item,
            Tag::Table(alignments) => Container::Table(alignments),
            Tag::TableHead => Container::TableHead,
            Tag::TableRow => Container::TableRow,
            Tag::TableCell => Container::TableCell,
            Tag::Emphasis => Container::Emphasis,
            Tag::Strong => Container::Strong,
            Tag::Strikethrough => Container::Strikethrough,
            Tag::Link { dest_url, .. } => self.anchor(&dest_url),
            // Only the alt text of an image is shown; the constructs the enabled extensions do
            // not produce keep their content.
            Tag::Image { .. }
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => Container::Plain,
        }
    }

    /// The anchor for a destination, or the plain text of the link when it is not one to
    /// follow. The destination is masked first: a secret in a URL is not a link target.
    fn anchor(&mut self, destination: &str) -> Container {
        if let Ok(url) = SafeUrl::parse(redact(destination).as_str()) {
            Container::Anchor(url)
        } else {
            self.withheld = self.withheld.saturating_add(1);
            Container::Plain
        }
    }

    fn close(&mut self) {
        let Some(Frame {
            container,
            children,
        }) = self.stack.pop()
        else {
            tracing::error!("a markdown end event came with no open container");
            return;
        };
        let inner = html! { @for child in &children { (child) } };
        let node = match container {
            Container::Paragraph => html! { p { (inner) } },
            Container::Heading(Heading::Third) => html! { h3 { (inner) } },
            Container::Heading(Heading::Fourth) => html! { h4 { (inner) } },
            Container::BlockQuote => html! { blockquote { (inner) } },
            Container::CodeBlock => html! { pre { code { (inner) } } },
            Container::RawBlock => html! { pre.raw { (inner) } },
            Container::List(Numbering::Bulleted) => html! { ul { (inner) } },
            Container::List(Numbering::From(1)) => html! { ol { (inner) } },
            Container::List(Numbering::From(first)) => html! { ol start=(first) { (inner) } },
            Container::Item => html! { li { (inner) } },
            Container::Table(_) => html! { table { (inner) } },
            Container::TableHead => html! { thead { tr { (inner) } } },
            Container::TableRow => html! { tr { (inner) } },
            Container::TableCell => self.cell(&inner),
            Container::Emphasis => html! { em { (inner) } },
            Container::Strong => html! { strong { (inner) } },
            Container::Strikethrough => html! { del { (inner) } },
            Container::Anchor(url) => external_link(&url, &inner),
            Container::Plain => inner,
        };
        self.push(node);
    }

    /// A table cell: a header cell inside the head, aligned as its column is.
    fn cell(&self, inner: &Markup) -> Markup {
        let in_head = self
            .stack
            .last()
            .is_some_and(|row| matches!(row.container, Container::TableHead));
        let column = self.stack.last().map_or(0, |row| row.children.len());
        let alignment = self.stack.iter().rev().find_map(|frame| {
            if let Container::Table(alignments) = &frame.container {
                alignments.get(column).copied()
            } else {
                None
            }
        });
        let class = match alignment {
            Some(Alignment::Left) => Some("left"),
            Some(Alignment::Center) => Some("center"),
            Some(Alignment::Right) => Some("right"),
            Some(Alignment::None) | None => None,
        };
        if in_head {
            html! { th class=[class] { (inner) } }
        } else {
            html! { td class=[class] { (inner) } }
        }
    }

    /// A text run: redacted again, because decoding entities and escapes can write a
    /// credential the source did not hold, and linked unless it is code, inside an anchor, or
    /// raw HTML.
    fn text(&mut self, text: &str) -> Markup {
        let text = redact(text);
        if self.linkable() {
            self.linked(&text)
        } else {
            html! { (text) }
        }
    }

    /// Inline code: as written, unless the whole of it is a local file the site shows, which
    /// is how agents usually write a path.
    fn code(&self, code: &Redacted<String>) -> Markup {
        let code_markup = html! { code { (code) } };
        match self
            .extractor
            .local_file(code.as_str())
            .filter(|_| self.linkable())
        {
            Some(file) => file_link(&file, &code_markup),
            None => code_markup,
        }
    }

    fn linkable(&self) -> bool {
        !self.stack.iter().any(|frame| {
            matches!(
                frame.container,
                Container::Anchor(_) | Container::CodeBlock | Container::RawBlock
            )
        })
    }

    fn linked(&mut self, text: &Redacted<String>) -> Markup {
        let found = self
            .extractor
            .segments(text.as_str(), &LinkContext::default());
        self.withheld = self.withheld.saturating_add(found.dropped);
        let pieces: Vec<Markup> = found
            .segments
            .iter()
            .map(|segment| self.segment(segment))
            .collect();
        html! { @for piece in &pieces { (piece) } }
    }

    fn segment(&mut self, segment: &Segment<'_>) -> Markup {
        match segment {
            Segment::Text(text) => html! { (text) },
            Segment::Link {
                link,
                shown,
                source,
            } => {
                if let Ok(url) = link.url(self.targets) {
                    external_link(&url, &html! { (shown) })
                } else {
                    self.withheld = self.withheld.saturating_add(1);
                    html! { (source) }
                }
            }
            Segment::File { file, source } => file_link(file, &html! { (source) }),
        }
    }

    fn push(&mut self, markup: Markup) {
        match self.stack.last_mut() {
            Some(frame) => frame.children.push(markup),
            None => self.root.push(markup),
        }
    }

    fn finish(mut self) -> Rendered {
        while !self.stack.is_empty() {
            self.close();
        }
        let blocks = self.root;
        Rendered {
            markup: html! { @for block in &blocks { (block) } },
            withheld: self.withheld,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The renderer on its own, with a placeholder configuration (`example.atlassian.net`,
    //! `example.slack.com`, `ABC`). A property test throws markdown-ish text at it, built from
    //! pieces that are dangerous in a page (script tags, `javascript:` links, images, raw HTML,
    //! entities that decode to markup), and checks what no input may produce: a tag outside
    //! the set the renderer draws, and a link that is not `https://`. One hand-written fixture
    //! is pinned as a snapshot, which pins the output as it is and is not derived by hand.

    use std::collections::HashSet;
    use std::path::Path;

    use proptest::prelude::*;
    use regex::Regex;

    use super::*;
    use crate::config::Config;

    fn config() -> Config {
        Config::parse(
            "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n[slack]\nhost = \"example.slack.com\"\n[files]\nroots = [\"/home/dev\"]\n",
            Path::new("/cfg.toml"),
            Path::new("/home/dev"),
        )
        .unwrap()
    }

    fn html_of(source: &str) -> Rendered {
        let config = config();
        let extractor = Extractor::from_config(&config).unwrap();
        render(&extractor, &config.link_targets(), &Redacted::of(source))
    }

    /// Every element the renderer can draw.
    const DRAWN: [&str; 21] = [
        "p",
        "h3",
        "h4",
        "blockquote",
        "pre",
        "code",
        "ul",
        "ol",
        "li",
        "table",
        "thead",
        "tr",
        "th",
        "td",
        "em",
        "strong",
        "del",
        "a",
        "br",
        "hr",
        "input",
    ];

    /// Pieces that are dangerous, or that markdown reads in its own way.
    const PIECES: [&str; 32] = [
        "/home/dev/a.md",
        "`/home/dev/b.md`",
        "[f](/home/dev/c.md)",
        "`/home/dev/q\"><i>.md`",
        "<script>alert(1)</script>",
        "<SCRIPT src=x>",
        "&lt;script&gt;",
        "&#60;script&#62;alert(1)",
        "&#x3c;img src=x onerror=1&#x3e;",
        "[a](javascript:alert(1))",
        "[b](https://example.com/y)",
        "[c](http://example.com/y)",
        "[d](data:text/html,<script>1</script>)",
        "<https://example.com/auto>",
        "<javascript:alert(1)>",
        "![i](https://example.com/x.png)",
        "![j](javascript:1)",
        "```\n<script>\n```",
        "    <script>indented</script>",
        "> quoted <b>",
        "- item",
        "1. first",
        "- [x] done",
        "| a | b |\n|---|---|\n| <script> | [e](javascript:1) |",
        "# heading <script>",
        "***",
        "**bold**",
        "`code <script>`",
        "ABC-123",
        "https://example.slack.com/archives/C0123ABCD/p1700000000000100",
        "http://plain.example/x",
        "<img src=x onerror=alert(1)>",
    ];

    fn markdownish() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            prop::sample::select(PIECES.to_vec()).prop_map(str::to_owned),
            ".{0,12}",
        ];
        let separator = prop::sample::select(vec!["", " ", "\n", "\n\n", "\n  "]);
        prop::collection::vec((piece, separator), 0..24).prop_map(|parts| {
            parts
                .into_iter()
                .flat_map(|(piece, separator)| [piece, separator.to_owned()])
                .collect()
        })
    }

    proptest! {
        /// Mutation: render `Event::Html`/`InlineHtml` without escaping (a `<script` or a tag
        /// outside `DRAWN` appears), build an anchor from the destination without `SafeUrl`
        /// (a `javascript:` href appears), or draw an image as `<img>`.
        #[test]
        fn no_input_makes_a_tag_the_renderer_does_not_draw_or_a_link_that_is_not_https(
            source in markdownish()
        ) {
            let html = html_of(&source).markup.into_string();
            prop_assert!(!html.to_ascii_lowercase().contains("<script"), "{html}");
            let tags = Regex::new(r"</?([A-Za-z][A-Za-z0-9]*)").unwrap();
            let drawn: HashSet<&str> = DRAWN.into_iter().collect();
            for tag in tags.captures_iter(&html) {
                prop_assert!(drawn.contains(&tag[1]), "tag {} in {html}", &tag[1]);
            }
            let hrefs = Regex::new(r#"href="([^"]*)""#).unwrap();
            let file_href = Regex::new(r"^/file\?path=[A-Za-z0-9%]+$").unwrap();
            for href in hrefs.captures_iter(&html) {
                prop_assert!(
                    href[1].starts_with("https://") || file_href.is_match(&href[1]),
                    "{} in {html}",
                    &href[1]
                );
            }
        }
    }

    /// The links the text names that the page cannot follow are counted, once each.
    /// Mutation: drop `self.withheld` from `anchor` (the `[bad]` row), or from `linked` (the
    /// bare `http://` row).
    #[test]
    fn links_that_are_refused_are_counted_and_shown_as_text() {
        let rendered = html_of("[bad](javascript:alert(1)) and http://plain.example/x");
        assert_eq!(rendered.withheld, 2);
        assert_eq!(
            rendered.markup.into_string(),
            "<p>bad and http://plain.example/x</p>"
        );
    }

    /// Decoding an entity can write a credential the source did not hold; the text shown is
    /// redacted after decoding.
    /// Mutation: skip `redact` in `text` (the token appears).
    #[test]
    fn a_credential_written_with_entities_is_masked_after_decoding() {
        let token = format!("ghp_{}", "a1B2".repeat(9));
        let encoded = token.replacen("gh", "&#103;h", 1);
        assert!(!Redacted::of(&encoded).as_str().contains("[redacted]"));
        let rendered = html_of(&format!("key {encoded} end"));
        assert_eq!(rendered.markup.into_string(), "<p>key [redacted] end</p>");
    }

    /// A path under a root opens the file, in prose or as a whole code span; a path outside
    /// the roots, a code span with more in it, and a path inside a link stay as written.
    /// Mutation: render `Event::Code` as plain code (the code-span row), drop the
    /// `Segment::File` arm's link (the prose row), or drop `linkable` from `code` (the link
    /// gains a nested anchor).
    #[test]
    fn a_local_file_path_opens_the_file() {
        let link = |label: &str, path: &str| {
            format!(
                r#"<a class="file-link" href="/file?path={}" title="{path}">{label}</a>"#,
                path.replace('/', "%2F")
                    .replace('.', "%2E")
                    .replace('-', "%2D")
            )
        };
        let html = |source: &str| html_of(source).markup.into_string();
        assert_eq!(
            html("brief at `/home/dev/s/plan-brief.md`."),
            format!(
                "<p>brief at {}.</p>",
                link(
                    "<code>/home/dev/s/plan-brief.md</code>",
                    "/home/dev/s/plan-brief.md"
                )
            )
        );
        assert_eq!(
            html("see /home/dev/a.md now"),
            format!(
                "<p>see {} now</p>",
                link("/home/dev/a.md", "/home/dev/a.md")
            )
        );
        assert_eq!(
            html("`/etc/a.md` and `cat /home/dev/a.md`"),
            "<p><code>/etc/a.md</code> and <code>cat /home/dev/a.md</code></p>"
        );
        assert_eq!(
            html("[`/home/dev/a.md`](https://example.com/x)"),
            r#"<p><a href="https://example.com/x" target="_blank" rel="noopener noreferrer"><code>/home/dev/a.md</code></a></p>"#
        );
    }

    /// Mutation: nest the stack as recursion (a stack overflow aborts the run).
    #[test]
    fn deeply_nested_quotes_do_not_exhaust_the_stack() {
        let source = format!("{}deep", "> ".repeat(5_000));
        let html = html_of(&source).markup.into_string();
        assert!(html.contains("deep"));
    }

    /// A hand-written fixture covering every construct the renderer draws. The snapshot pins
    /// today's output; it was read through once against the fixture.
    #[test]
    fn the_fixture_renders_as_pinned() {
        let fixture = "\
# Plan

Some **bold**, *italic*, ~~struck~~ and `code` text,
with a soft break and a hard break  
here. See ABC-123 and [the design](https://example.com/design), or
https://example.slack.com/archives/C0123ABCD/p1700000000000100 and
https://github.com/example-org/example-repo/pull/42.

## Steps

1. first
2. second

- [x] done
- [ ] open
- plain

> quoted *text*

| Name | Count |
|:-----|------:|
| a    | 1     |

```
raw <b>code</b>
```

---

![diagram](https://example.com/d.png) <b>raw</b> [bad](javascript:alert(1))
";
        let rendered = html_of(fixture);
        assert_eq!(rendered.withheld, 1);
        insta::assert_snapshot!(
            rendered.markup.into_string(),
            @r#"
<h3>Plan</h3><p>Some <strong>bold</strong>, <em>italic</em>, <del>struck</del> and <code>code</code> text,
with a soft break and a hard break<br>here. See <a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a> and <a href="https://example.com/design" target="_blank" rel="noopener noreferrer">the design</a>, or
<a href="https://example.slack.com/archives/C0123ABCD/p1700000000000100" target="_blank" rel="noopener noreferrer">https://example.slack.com/archives/C0123ABCD/p1700000000000100</a> and
<a href="https://github.com/example-org/example-repo/pull/42" target="_blank" rel="noopener noreferrer">https://github.com/example-org/example-repo/pull/42</a>.</p><h4>Steps</h4><ol><li>first</li><li>second</li></ol><ul><li><input type="checkbox" disabled checked> done</li><li><input type="checkbox" disabled> open</li><li>plain</li></ul><blockquote><p>quoted <em>text</em></p></blockquote><table><thead><tr><th class="left">Name</th><th class="right">Count</th></tr></thead><tr><td class="left">a</td><td class="right">1</td></tr></table><pre><code>raw &lt;b&gt;code&lt;/b&gt;
</code></pre><hr><p>diagram &lt;b&gt;raw&lt;/b&gt; bad</p>
"#
        );
    }
}
