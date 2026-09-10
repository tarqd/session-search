//! Splitting a markdown message body into prose, code and headings.
//!
//! Transcript messages are markdown, and the two halves of one message want opposite
//! analysis. A paragraph wants stemming, so `compiling` finds `compiled`; a fenced block
//! wants the `code` analyzer, which splits identifiers and never stems — stemming a
//! snippet turns `Serializes` into `serial` and `parses` into `pars`, which matches
//! nothing anyone typed. Indexing both halves under one analyzer means picking which of
//! the two searches to break.
//!
//! [`split`] walks pulldown-cmark's event stream once and routes each event:
//!
//! | markdown | lands in |
//! | --- | --- |
//! | paragraph, list item, table cell, blockquote, link text, image alt, raw HTML | `text` |
//! | fenced or indented code block | one `code` entry (+ its info word in `code_langs`) |
//! | inline `` `span` `` | one `code` entry **and** stays in `text` |
//! | heading | one `headings` entry **and** an entry of `text` |
//!
//! Two of those routings are worth stating out loud:
//!
//! * A heading is prose, so routing it *only* to its own field would take the section title
//!   out of the sentence flow a `text` query searches. It is cheap to carry in both: headings
//!   are short, and the boosted `headings` field is what makes the ranking difference.
//! * Raw HTML is kept as text rather than dropped. A message that opens with
//!   `<system-reminder>` is one HTML block to a markdown parser, so dropping HTML would
//!   silently unindex the whole of it.
//!
//! Link *destinations* are dropped: a URL is not prose, and `tool_input` already carries the
//! paths anyone searches for.
//!
//! Prose comes back as one entry *per block*, never as one joined string. Lifting a fence out
//! of the middle of a message would otherwise close the gap it left and make the sentence
//! before it adjacent to the sentence after it, so a phrase query would match across text that
//! was never adjacent. Tantivy separates the values of a multi-valued field by a position gap,
//! which is exactly the gap the removed block should leave behind.
//!
//! Malformed input is not a special case — an unclosed fence, a stray `|`, a heading with no
//! text — because pulldown-cmark is a total function over `&str`: every `Start` it emits is
//! matched by an `End` at EOF, and it never fails. The tests pin that rather than trust it.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// The typed pieces of one markdown body. Every field may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownParts {
    /// Prose, **one entry per block**, in document order.
    ///
    /// One entry per block rather than one joined string, because the blocks are no longer
    /// adjacent once the code between them has been routed elsewhere: Tantivy puts a position
    /// gap between the values of a multi-valued field, so a phrase cannot run from the end of
    /// one paragraph into the start of the next across a fence that was lifted out.
    pub text: Vec<String>,
    /// One entry per code block, plus one per inline span.
    pub code: Vec<String>,
    /// One entry per heading, in document order.
    pub headings: Vec<String>,
    /// Info-string languages of the fenced blocks, deduped in first-seen order.
    pub code_langs: Vec<String>,
}

/// Split a markdown body into prose, code and headings.
///
/// Pure, allocation-light and single-pass: no regex, one `String` per block that survives, and
/// the parser itself borrows from `markdown` rather than copying it.
pub fn split(markdown: &str) -> MarkdownParts {
    // Tables and strikethrough are the two extensions transcripts actually use. Everything
    // else stays off: an unrecognised extension syntax degrades to text, which is where it
    // would have gone anyway.
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut out = MarkdownParts::default();
    // The prose block being built, or the heading being built.
    let mut block = String::new();
    // The code block being built.
    let mut code = String::new();
    let mut in_code = false;

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut block, &mut out.text);
                in_code = true;
                code.clear();
                if let CodeBlockKind::Fenced(info) = kind
                    && let Some(lang) = info_word(&info)
                    && !out.code_langs.contains(&lang)
                {
                    out.code_langs.push(lang);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                let body = std::mem::take(&mut code);
                let body = body.trim_end_matches('\n');
                if !body.trim().is_empty() {
                    out.code.push(body.to_string());
                }
            }
            Event::Start(Tag::Heading { .. }) => flush(&mut block, &mut out.text),
            Event::End(TagEnd::Heading(_)) => {
                let heading = block.trim().to_string();
                block.clear();
                if !heading.is_empty() {
                    out.headings.push(heading.clone());
                    out.text.push(heading);
                }
            }
            // An inline span is code *and* prose: it is the identifier a `code` search wants,
            // and removing it from the sentence would leave a hole in the sentence.
            Event::Code(span) => {
                if !span.trim().is_empty() {
                    out.code.push(span.to_string());
                }
                block.push_str(&span);
            }
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                if in_code {
                    code.push_str(&t);
                } else {
                    block.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code {
                    code.push('\n');
                } else {
                    block.push('\n');
                }
            }
            // Every construct that ends a run of prose. A blockquote's own text arrives as
            // paragraphs inside it, so the outer end is only a safety net.
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Item
                | TagEnd::TableCell
                | TagEnd::TableHead
                | TagEnd::TableRow
                | TagEnd::BlockQuote(_)
                | TagEnd::HtmlBlock,
            ) => flush(&mut block, &mut out.text),
            _ => {}
        }
    }
    flush(&mut block, &mut out.text);
    out
}

/// Close the block being built, keeping it only if it holds something.
fn flush(block: &mut String, blocks: &mut Vec<String>) {
    let trimmed = block.trim();
    if !trimmed.is_empty() {
        blocks.push(trimmed.to_string());
    }
    block.clear();
}

/// The language of a fence's info string: its first word, lowercased.
///
/// A comma ends the word as well as whitespace, so the `rust` of ```` ```rust,ignore ```` is
/// one `code_lang` bucket rather than a family of near-duplicates.
fn info_word(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?;
    let word = word.split(',').next().unwrap_or(word);
    (!word.is_empty()).then(|| word.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Indexing notes

The indexer had already **compiled** the schema, so `open_or_create` reused it.

## Steps

- run `cargo build`
- read the [design doc](docs/DESIGN.md)

| field | tokenizer |
| --- | --- |
| text | prose |
| code | code |

```rust
pub fn open_or_create(dir: &Path) -> Result<Index> {
    Index::open_in_dir(dir)
}
```

> A quoted remark about tantivy.
";

    /// The prose blocks as one string, for the assertions that only care that a phrase
    /// survived the split and not which block it landed in.
    fn prose(parts: &MarkdownParts) -> String {
        parts.text.join("\n")
    }

    #[test]
    fn a_sample_body_splits_into_prose_code_and_headings() {
        let parts = split(SAMPLE);
        let text = prose(&parts);

        assert_eq!(parts.headings, ["Indexing notes", "Steps"]);
        assert_eq!(parts.code_langs, ["rust"]);

        // Prose keeps the sentences, the list items, the table cells, the link *text* and the
        // blockquote — and the headings, which `show` prints.
        assert!(text.contains("Indexing notes"));
        assert!(text.contains("had already compiled the schema"));
        assert!(text.contains("run cargo build"));
        assert!(text.contains("read the design doc"));
        assert!(text.contains("tokenizer"));
        assert!(text.contains("A quoted remark about tantivy."));
        // ...but not the link destination, and not the fenced block.
        assert!(!text.contains("docs/DESIGN.md"), "{:?}", parts.text);
        assert!(!text.contains("Index::open_in_dir"), "{:?}", parts.text);

        // The fence is one entry; the inline spans are entries of their own and stay in prose.
        assert_eq!(parts.code.len(), 3, "{:?}", parts.code);
        assert!(parts.code.contains(&"open_or_create".to_string()));
        assert!(parts.code.contains(&"cargo build".to_string()));
        assert!(text.contains("open_or_create"));
        let fence = parts
            .code
            .iter()
            .find(|c| c.contains("pub fn"))
            .expect("the fenced block is one entry");
        assert!(fence.starts_with("pub fn open_or_create"), "{fence:?}");
        assert!(fence.ends_with('}'), "{fence:?}");
    }

    #[test]
    fn an_indented_block_is_code_with_no_language() {
        let parts = split("intro\n\n    let x = 1;\n    let y = 2;\n");
        assert_eq!(parts.text, ["intro"]);
        assert_eq!(parts.code, ["let x = 1;\nlet y = 2;"]);
        assert!(parts.code_langs.is_empty());
    }

    #[test]
    fn an_info_string_contributes_its_first_word_lowercased() {
        assert_eq!(split("```Rust\nx\n```").code_langs, ["rust"]);
        assert_eq!(split("```  BASH  \nx\n```").code_langs, ["bash"]);
        assert_eq!(split("```rust,ignore\nx\n```").code_langs, ["rust"]);
        assert_eq!(split("```js title=a.js\nx\n```").code_langs, ["js"]);
        assert!(split("```\nx\n```").code_langs.is_empty());
        // One bucket per language, however many blocks use it.
        assert_eq!(
            split("```py\na\n```\n\n```py\nb\n```").code_langs,
            ["py".to_string()]
        );
    }

    #[test]
    fn an_unclosed_fence_does_not_panic_and_keeps_its_body() {
        let parts = split("before\n\n```rust\nfn main() {}\nstill inside\n");
        assert_eq!(parts.text, ["before"]);
        assert_eq!(parts.code, ["fn main() {}\nstill inside"]);
        assert_eq!(parts.code_langs, ["rust"]);
    }

    #[test]
    fn malformed_input_is_split_rather_than_rejected() {
        // A heading with no text, a bare table pipe, an unterminated span, a lone fence.
        for input in [
            "#",
            "##   \n",
            "| a | b\n|---\n| c",
            "text with `an unclosed span",
            "```",
            "```\n",
            "~~~~~\n???",
            "> \n> \n",
            "- \n- \n",
            "\u{0}\u{1}\u{feff}",
            "***",
        ] {
            let parts = split(input);
            // The invariant that matters downstream: nothing is a blank entry.
            assert!(
                parts.code.iter().all(|c| !c.trim().is_empty()),
                "blank code entry from {input:?}"
            );
            assert!(
                parts.headings.iter().all(|h| !h.trim().is_empty()),
                "blank heading from {input:?}"
            );
            assert!(
                parts.code_langs.iter().all(|l| !l.is_empty()),
                "blank lang from {input:?}"
            );
        }
    }

    #[test]
    fn empty_input_yields_empty_parts() {
        assert_eq!(split(""), MarkdownParts::default());
        assert_eq!(split("   \n\n  \t\n"), MarkdownParts::default());
    }

    #[test]
    fn plain_prose_passes_through_unchanged_except_for_block_joins() {
        let parts = split("just a sentence, no markup at all.");
        assert_eq!(parts.text, ["just a sentence, no markup at all."]);
        assert!(parts.code.is_empty());
        assert!(parts.headings.is_empty());
    }

    #[test]
    fn raw_html_stays_in_the_text_it_wraps() {
        // A markdown parser sees this as one HTML block; dropping HTML would unindex it all.
        let parts = split("<system-reminder>\nthe budget is 15000 tokens\n</system-reminder>");
        assert!(
            prose(&parts).contains("the budget is 15000 tokens"),
            "{parts:?}"
        );
        assert!(parts.code.is_empty());
    }

    #[test]
    fn image_alt_text_is_kept_and_its_url_is_not() {
        let parts = split("![a flame graph of the indexer](/tmp/flame.svg)");
        assert_eq!(parts.text, ["a flame graph of the indexer"]);
    }

    #[test]
    fn each_prose_block_is_its_own_entry() {
        let parts = split("first para\n\nsecond para\n\n- item one\n- item two\n");
        assert_eq!(
            parts.text,
            ["first para", "second para", "item one", "item two"]
        );
    }

    /// The reason the entries stay separate: what sat between two of them is gone, and the
    /// position gap a multi-valued field inserts is what stands in for it.
    #[test]
    fn a_removed_block_leaves_the_prose_on_either_side_of_it_in_separate_entries() {
        let parts = split(
            "Call it before the writer exists.\n\n```rust\nlet x = 1;\n```\n\nThen re-run the tests.",
        );
        assert_eq!(
            parts.text,
            [
                "Call it before the writer exists.",
                "Then re-run the tests."
            ]
        );
        // The same for a heading, which is prose *and* a heading.
        assert_eq!(split("# Title one\n\nAlpha ends here.").text.len(), 2);
    }
}
