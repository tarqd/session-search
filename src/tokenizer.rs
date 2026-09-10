//! The two analyzers: `code`, which indexes identifiers the way people search for them, and
//! `prose`, which indexes English the way people search for that.
//!
//! `parse.rs` splits a markdown message into its prose and its code (see `markdown.rs`) so each
//! half can have the analyzer it wants: `text` and `headings` are `prose`, while `code`,
//! `thinking` and `tool_input` are `code`. The rest of this module is the `code` analyzer;
//! [`prose_analyzer`] is that same analyzer with an English stemmer on the end, and is
//! documented at its definition.
//!
//! Transcripts are mostly code, paths and shell, and the stock `default` analyzer answers badly
//! for them: it indexes `SnippetGenerator` as one opaque blob, so `snippet` never finds it; it
//! offers no way back from `OpenOrCreate` to `open_or_create`, since the two spellings share no
//! term; and its 40-byte `RemoveLongFilter` silently drops every sha256. This module fixes all
//! three by emitting, for every token, **the whole identifier plus each of its parts at the same
//! position** — the classic synonym trick, which leaves phrase queries working because the
//! sub-parts never consume a position of their own.
//!
//! ```text
//! open_or_create  ->  openorcreate, open, or, create        (all at position p)
//! SnippetGenerator ->  snippetgenerator, snippet, generator (all at position p)
//! parseTs2Ms      ->  parsets2ms, parse, ts, 2, ms          (all at position p)
//! cargo           ->  cargo                                 (a plain word emits once)
//! ```
//!
//! Two consequences are load-bearing, and both are why the whole form is the *separator-free*
//! lowercasing rather than the verbatim one:
//!
//! * Tantivy's `QueryParser` turns a query word that yields several tokens into a `PhraseQuery`
//!   over `(position, term)` pairs. Terms sharing a position must therefore all be present at
//!   that same position in the document, so a query matches an identifier only when the two
//!   sides produce the *same* set of terms. `open_or_create` and `OpenOrCreate` both collapse
//!   to `openorcreate` + `open`/`or`/`create`, which is exactly what makes each spelling find
//!   the other. Keeping `open_or_create` verbatim would break that symmetry in one direction.
//! * `snake_case` and `camelCase` spellings of the same name become interchangeable for free,
//!   and `iserror` finds `is_error`.
//!
//! The base tokenizer is [`WordTokenizer`], a hand-written scan, rather than Tantivy's
//! `SimpleTokenizer`, which splits on `_` before any filter can see it, so an
//! underscore-aware filter sitting behind it would be dead code and `OpenOrCreate` could never
//! find `open_or_create`. A token is a run of letters, digits and `_`; everything else
//! separates, so `src/index.rs` still yields `src`, `index`, `rs`. A `\w+` `RegexTokenizer`
//! draws the same boundaries, but it runs the regex engine once per token and clones the
//! compiled regex for every field value — measured at roughly fourteen times the scan cost of
//! `SimpleTokenizer` over real transcript text, and the dominant part of this analyzer's bill.
//!
//! [`RemoveLongFilter`] runs last with a 255-byte limit instead of the stock 40, so a sha256 or
//! a long generated identifier stays findable instead of being silently dropped.
//!
//! Hashes are not identifiers, so they are never split: a run of eight or more hex digits that
//! mixes letters with digits, or any token that shatters into a crowd of one- and two-character
//! fragments, is emitted whole and alone. Splitting a sha256 buys nothing anyone would search for and costs 41 junk terms —
//! single characters among them — that pollute the dictionary and inflate the BM25 length of
//! every document holding a hash.
//!
//! Changing anything here changes the terms on disk. Renaming the analyzer changes the schema
//! too — the field's tokenizer name is part of it — so `index::open_or_create` discards and
//! rebuilds the index on the next run; changing only the *behaviour* under the same name does
//! not, and leaves the old terms in place until something else forces a reindex.

use std::str::CharIndices;

use tantivy::Index;
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, Stemmer, TextAnalyzer, Token, TokenFilter, TokenStream,
    Tokenizer,
};

/// The name the analyzer is registered under, and the name the schema refers to.
pub const CODE_ANALYZER: &str = "code";

/// The prose analyzer's name, for the fields that hold English rather than identifiers.
pub const PROSE_ANALYZER: &str = "prose";

/// Tokens longer than this are dropped. The stock limit is 40, which loses every sha256.
const MAX_TOKEN_BYTES: usize = 255;

/// A token of at least this many hex digits is a hash, a blob id or a chunk of a UUID rather
/// than a name, and is indexed whole instead of being split.
const MIN_HEX_BLOB_BYTES: usize = 8;

/// More parts than this, most of them only a character or two long, is the same story for a
/// token that is not pure hex: machine noise, not an identifier anyone reads.
const MAX_IDENT_PARTS: usize = 6;

/// Build the `code` analyzer. Cheap enough to call per index; `TextAnalyzer` is `Clone`.
pub fn code_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(SplitIdentifiers)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .build()
}

/// Build the `prose` analyzer: the one the *other* half of a message wants.
///
/// It is the `code` analyzer plus an English stemmer, and the stemmer is the whole difference:
/// `compiling` finds `compiled`. Stemming is exactly what must never touch a snippet — it
/// turns `Serializes` into `serial` and `parses` into `pars`, terms no reader would type —
/// which is why the two analyzers exist and why a message is split between their two fields.
///
/// What it does **not** drop is identifier splitting. Only fenced blocks and inline spans are
/// routed to `code`; a sentence that names `SnippetGenerator` without backticks stays here, as
/// does every attachment, `system` record and tool call, none of which is markdown to split at
/// all. Tokenizing those the stock way would index `SnippetGenerator` as one opaque word again
/// and leave `snippet` unable to find it — the exact hole [`code_analyzer`] was written to
/// close. So prose keeps the whole-plus-parts trick and merely stems what comes out of it: the
/// parts share the whole's position either way, so phrases still work, and a plain English
/// word emits once, stemmed, exactly as before.
///
/// The `LowerCaser` runs before the stemmer because `Stemmer` matches on lowercase input, and
/// `RemoveLongFilter` runs last so a token is measured as it will be indexed.
pub fn prose_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(SplitIdentifiers)
        .filter(LowerCaser)
        .filter(Stemmer::new(Language::English))
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .build()
}

/// Register the `code` and `prose` analyzers on `index`.
///
/// Must be called on **every** path that opens or creates an [`Index`], before any document is
/// added and before any query is parsed: both the writer and `QueryParser` look the analyzer up
/// by name, and a missing registration is an error at that point, not at open time.
pub fn register(index: &Index) {
    index.tokenizers().register(CODE_ANALYZER, code_analyzer());
    index
        .tokenizers()
        .register(PROSE_ANALYZER, prose_analyzer());
}

/// A RAM index carrying the pinned schema **and** both analyzers — the in-memory
/// counterpart of `index::open_or_create`, so no test can forget to register them and then fail
/// with `UnknownTokenizer` a dozen frames deep.
#[cfg(test)]
pub(crate) fn create_in_ram(schema: tantivy::schema::Schema) -> Index {
    let index = Index::create_in_ram(schema);
    register(&index);
    index
}

// ---------------------------------------------------------------------------
// the base tokenizer
// ---------------------------------------------------------------------------

/// Splits text into runs of letters, digits and `_`; everything else separates.
///
/// The same boundaries a `\w+` regex draws, at the cost of a `char` comparison per byte instead
/// of a regex search per token.
#[derive(Clone, Default)]
pub struct WordTokenizer {
    token: Token,
}

pub struct WordTokenStream<'a> {
    text: &'a str,
    chars: CharIndices<'a>,
    token: &'a mut Token,
}

/// Letters, digits and `_` make up a token; everything else separates two.
fn is_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = WordTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordTokenStream<'a> {
        self.token.reset();
        WordTokenStream {
            text,
            chars: text.char_indices(),
            token: &mut self.token,
        }
    }
}

impl TokenStream for WordTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        // `Token::reset` leaves the position at `usize::MAX`, so the first token lands on 0.
        self.token.position = self.token.position.wrapping_add(1);
        while let Some((offset_from, c)) = self.chars.next() {
            if !is_word(c) {
                continue;
            }
            let offset_to = self
                .chars
                .find(|(_, c)| !is_word(*c))
                .map_or(self.text.len(), |(offset, _)| offset);
            self.token.offset_from = offset_from;
            self.token.offset_to = offset_to;
            self.token.text.push_str(&self.text[offset_from..offset_to]);
            return true;
        }
        false
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

// ---------------------------------------------------------------------------
// the filter
// ---------------------------------------------------------------------------

/// Splits identifiers into their parts and emits them alongside the whole, at one position.
///
/// Case is left alone here; the `LowerCaser` that follows lowercases every token this emits,
/// so the analyzer as a whole yields lowercase terms.
#[derive(Clone)]
pub struct SplitIdentifiers;

impl TokenFilter for SplitIdentifiers {
    type Tokenizer<T: Tokenizer> = SplitIdentifiersFilter<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> SplitIdentifiersFilter<T> {
        SplitIdentifiersFilter {
            inner: tokenizer,
            pending: Vec::new(),
            parts: Vec::new(),
            token: Token::default(),
        }
    }
}

#[derive(Clone)]
pub struct SplitIdentifiersFilter<T> {
    inner: T,
    /// Tokens still owed for the current input token, in **reverse** emission order.
    pending: Vec<Token>,
    /// Scratch for `parts_of`, kept across tokens so a plain word costs no allocation.
    parts: Vec<(usize, usize)>,
    token: Token,
}

impl<T: Tokenizer> Tokenizer for SplitIdentifiersFilter<T> {
    type TokenStream<'a> = SplitIdentifiersStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.pending.clear();
        self.parts.clear();
        self.token.reset();
        SplitIdentifiersStream {
            tail: self.inner.token_stream(text),
            pending: &mut self.pending,
            parts: &mut self.parts,
            token: &mut self.token,
        }
    }
}

pub struct SplitIdentifiersStream<'a, T> {
    tail: T,
    pending: &'a mut Vec<Token>,
    parts: &'a mut Vec<(usize, usize)>,
    token: &'a mut Token,
}

impl<T: TokenStream> TokenStream for SplitIdentifiersStream<'_, T> {
    fn advance(&mut self) -> bool {
        loop {
            if let Some(next) = self.pending.pop() {
                *self.token = next;
                return true;
            }
            if !self.tail.advance() {
                return false;
            }
            expand(self.tail.token(), self.parts, self.pending);
        }
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

/// Push everything `token` should become onto `out`, in reverse emission order.
///
/// A token that is already a single plain word is pushed unchanged — no allocation, no extra
/// terms. Anything with a separator or a case/digit boundary is pushed as the whole identifier
/// (separators removed) followed by each part, all sharing `token.position`. A hash is pushed
/// whole and alone: see `is_blob`.
///
/// `parts` is scratch space owned by the caller; its contents on entry are irrelevant.
fn expand(token: &Token, parts: &mut Vec<(usize, usize)>, out: &mut Vec<Token>) {
    parts_of(&token.text, parts);

    // A plain word: one part covering the whole token, and nothing was stripped.
    if let [only] = parts.as_slice()
        && only.0 == 0
        && only.1 == token.text.len()
    {
        out.push(token.clone());
        return;
    }
    if parts.is_empty() {
        // The token was nothing but separators (`__`). It carries no term.
        return;
    }

    let part_token = |&(from, to): &(usize, usize)| Token {
        offset_from: token.offset_from + from,
        offset_to: token.offset_from + to,
        position: token.position,
        text: token.text[from..to].to_string(),
        position_length: 1,
    };

    // A single part with something stripped around it (`_foo`) needs no separate whole form.
    if let [only] = parts.as_slice() {
        out.push(part_token(only));
        return;
    }

    let mut text = String::with_capacity(token.text.len());
    for &(from, to) in parts.iter() {
        text.push_str(&token.text[from..to]);
    }
    let whole = Token {
        offset_from: token.offset_from,
        offset_to: token.offset_to,
        position: token.position,
        text,
        position_length: 1,
    };

    // A hash is not an identifier, and its fragments are nobody's search.
    if is_blob(&whole.text, parts) {
        out.push(whole);
        return;
    }

    // Parts first, reversed, so the whole identifier pops off the back first.
    for part in parts.iter().rev() {
        out.push(part_token(part));
    }
    out.push(whole);
}

/// Is this token machine noise — a sha256, a UUID chunk, a generated blob id — rather than a
/// name someone might search a part of?
///
/// Splitting one costs dozens of one- and two-character terms that match nothing anyone meant
/// (`f` would hit every sha256 in the corpus) and inflate the BM25 field length of the document
/// that carried the hash, since Tantivy counts tokens rather than positions. The whole form
/// still gets indexed, so pasting a hash back still finds the document it came from.
fn is_blob(whole: &str, parts: &[(usize, usize)]) -> bool {
    // Hex, long, and mixing letters with digits: a hash or an id. Letters alone (`beefcafe`)
    // never reach here — they are one part — and digits alone are a number, not a blob.
    if whole.len() >= MIN_HEX_BLOB_BYTES
        && whole.bytes().all(|b| b.is_ascii_hexdigit())
        && whole.bytes().any(|b| b.is_ascii_digit())
        && whole.bytes().any(|b| b.is_ascii_alphabetic())
    {
        return true;
    }
    // Not hex, but shattered all the same: a crowd of fragments, most of them a character or
    // two. A real identifier's parts are words.
    let short = parts.iter().filter(|(from, to)| to - from <= 2).count();
    parts.len() > MAX_IDENT_PARTS && short * 2 > parts.len()
}

/// Byte ranges of the sub-parts of one token, written into `parts` (cleared first).
///
/// Splits on `_`, on a lower/digit -> upper transition (`parseTs`), on the last upper of an
/// upper run followed by a word (`HTTPServer` -> `HTTP`, `Server`), and on any letter/digit
/// transition (`Ts2Ms` -> `Ts`, `2`, `Ms`).
fn parts_of(text: &str, parts: &mut Vec<(usize, usize)>) {
    parts.clear();
    let mut start = None::<usize>;
    let mut prev: Option<char> = None;

    for (i, c) in text.char_indices() {
        if c == '_' {
            if let Some(s) = start.take() {
                parts.push((s, i));
            }
            prev = Some(c);
            continue;
        }
        let boundary = match prev {
            Some(p) if p != '_' => is_boundary(p, c, &text[i..]),
            _ => false,
        };
        if boundary && let Some(s) = start.replace(i) {
            parts.push((s, i));
        }
        if start.is_none() {
            start = Some(i);
        }
        prev = Some(c);
    }
    if let Some(s) = start {
        parts.push((s, text.len()));
    }
}

/// Is there a part boundary *before* `cur`, which follows `prev` and heads `rest`?
fn is_boundary(prev: char, cur: char, rest: &str) -> bool {
    // camelCase / pascalCase: `eT` in `parseTs`.
    if !prev.is_uppercase() && prev.is_alphabetic() && cur.is_uppercase() {
        return true;
    }
    // An acronym meeting a word: the `S` of `HTTPServer`. It takes two lowercase letters to
    // make a word, because a lone trailing `s` belongs to the acronym — `getIDs` has to stay
    // `get` + `IDs` for a search for `ids` to find it.
    if prev.is_uppercase() && cur.is_uppercase() && word_follows(rest) {
        return true;
    }
    // Digits are their own part: `Ts2Ms`.
    prev.is_numeric() != cur.is_numeric()
}

/// Do at least two lowercase letters follow the first character of `rest`?
fn word_follows(rest: &str) -> bool {
    let mut after = rest.chars().skip(1);
    matches!((after.next(), after.next()), (Some(a), Some(b)) if a.is_lowercase() && b.is_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(text, position, offset_from, offset_to)` for every token the analyzer emits.
    fn analyze(text: &str) -> Vec<(String, usize, usize, usize)> {
        let mut analyzer = code_analyzer();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        stream.process(&mut |t: &Token| {
            out.push((t.text.clone(), t.position, t.offset_from, t.offset_to));
        });
        out
    }

    fn texts(text: &str) -> Vec<String> {
        analyze(text).into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn an_underscored_identifier_emits_the_whole_and_every_part_at_one_position() {
        assert_eq!(
            analyze("open_or_create"),
            vec![
                ("openorcreate".to_string(), 0, 0, 14),
                ("open".to_string(), 0, 0, 4),
                ("or".to_string(), 0, 5, 7),
                ("create".to_string(), 0, 8, 14),
            ]
        );
    }

    #[test]
    fn a_pascal_case_identifier_splits_on_the_case_boundary() {
        assert_eq!(
            analyze("SnippetGenerator"),
            vec![
                ("snippetgenerator".to_string(), 0, 0, 16),
                ("snippet".to_string(), 0, 0, 7),
                ("generator".to_string(), 0, 7, 16),
            ]
        );
    }

    #[test]
    fn digits_are_their_own_part() {
        assert_eq!(
            analyze("parseTs2Ms"),
            vec![
                ("parsets2ms".to_string(), 0, 0, 10),
                ("parse".to_string(), 0, 0, 5),
                ("ts".to_string(), 0, 5, 7),
                ("2".to_string(), 0, 7, 8),
                ("ms".to_string(), 0, 8, 10),
            ]
        );
    }

    #[test]
    fn a_plain_word_emits_once() {
        assert_eq!(analyze("cargo"), vec![("cargo".to_string(), 0, 0, 5)]);
        // ...and lowercasing alone is not a reason to emit twice.
        assert_eq!(analyze("Cargo"), vec![("cargo".to_string(), 0, 0, 5)]);
    }

    #[test]
    fn separators_advance_the_position_but_parts_do_not() {
        let positions: Vec<(String, usize)> = analyze("fn open_or_create(dir)")
            .into_iter()
            .map(|(text, position, _, _)| (text, position))
            .collect();
        assert_eq!(
            positions,
            vec![
                ("fn".to_string(), 0),
                ("openorcreate".to_string(), 1),
                ("open".to_string(), 1),
                ("or".to_string(), 1),
                ("create".to_string(), 1),
                ("dir".to_string(), 2),
            ]
        );
    }

    #[test]
    fn both_spellings_of_one_name_produce_the_same_terms() {
        let snake = texts("open_or_create");
        let pascal = texts("OpenOrCreate");
        let camel = texts("openOrCreate");
        assert_eq!(snake, pascal);
        assert_eq!(snake, camel);
    }

    #[test]
    fn an_acronym_keeps_its_head() {
        assert_eq!(
            texts("HTTPServerError"),
            vec!["httpservererror", "http", "server", "error"]
        );
    }

    #[test]
    fn a_trailing_plural_belongs_to_the_acronym() {
        // A lone lowercase `s` does not start a word, so `IDs` is not `I` + `Ds` and a search
        // for `ids` finds every one of these.
        assert_eq!(texts("getIDs"), vec!["getids", "get", "ids"]);
        assert_eq!(texts("userIDs"), vec!["userids", "user", "ids"]);
        assert_eq!(texts("parseURLs"), vec!["parseurls", "parse", "urls"]);
        assert_eq!(texts("IDs"), vec!["ids"]);
        // Two lowercase letters do start a word, so an acronym still meets one.
        assert_eq!(texts("HTTPServer"), vec!["httpserver", "http", "server"]);
    }

    #[test]
    fn a_sha256_survives_the_length_filter_whole_and_in_one_piece() {
        let hash = "a".repeat(64);
        assert_eq!(texts(&hash), vec![hash.clone()]);
        // A mixed hash is one term too: an exact paste still hits, and the 41 fragments a
        // letter/digit split would produce (`9`, `f`, `86`, ...) never reach the dictionary.
        let mixed = "0f3a".repeat(16);
        assert_eq!(texts(&mixed), vec![mixed.clone()]);
        assert_eq!(
            texts("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08").len(),
            1
        );
        // The chunks of a UUID are hashes as well.
        assert_eq!(
            texts("b20208d8-fbdb-5918-ba69-d203de6ed6dc"),
            vec![
                "b20208d8",
                "fbdb",
                "5918",
                "ba69",
                "ba",
                "69",
                "d203de6ed6dc"
            ]
        );
        // But a name that merely happens to hold digits is still an identifier.
        assert_eq!(
            texts("parse_utf8_len"),
            vec!["parseutf8len", "parse", "utf", "8", "len"]
        );
        assert_eq!(texts("sha256sum"), vec!["sha256sum", "sha", "256", "sum"]);
    }

    #[test]
    fn a_crowd_of_one_character_fragments_is_a_blob_and_not_a_name() {
        // Not hex, so the hash rule does not catch it — but nothing in it is a word either.
        assert_eq!(texts("z1x2y3z4w5v6"), vec!["z1x2y3z4w5v6"]);
        // A long snake_case name has as many parts, and every one of them is a word.
        assert_eq!(
            texts("open_or_create_the_index_dir_now"),
            vec![
                "openorcreatetheindexdirnow",
                "open",
                "or",
                "create",
                "the",
                "index",
                "dir",
                "now",
            ]
        );
    }

    #[test]
    fn a_token_over_the_limit_is_dropped_but_its_parts_survive() {
        let long = format!("{}_{}", "a".repeat(200), "b".repeat(200));
        assert_eq!(texts(&long), vec!["a".repeat(200), "b".repeat(200)]);
    }

    #[test]
    fn degenerate_tokens_do_not_panic_or_emit_empty_terms() {
        assert!(texts("___").is_empty());
        assert!(texts("").is_empty());
        assert_eq!(texts("_foo"), vec!["foo"]);
        assert_eq!(texts("foo_"), vec!["foo"]);
        assert!(texts(" - / ").is_empty());
        for text in ["a_b", "A", "1", "_", "a1b2", "ÉclairCafé", "日本語_text"] {
            for (term, ..) in analyze(text) {
                assert!(!term.is_empty(), "empty term from {text:?}");
            }
        }
    }

    #[test]
    fn punctuation_still_separates_positions() {
        assert_eq!(
            analyze("src/index.rs"),
            vec![
                ("src".to_string(), 0, 0, 3),
                ("index".to_string(), 1, 4, 9),
                ("rs".to_string(), 2, 10, 12),
            ]
        );
    }

    #[test]
    fn offsets_point_back_into_the_original_text() {
        let text = "call parseTs2Ms now";
        for (term, _, from, to) in analyze(text) {
            let slice = &text[from..to];
            assert!(
                slice.to_lowercase().replace('_', "").contains(&term)
                    || term == slice.to_lowercase().replace('_', ""),
                "term {term:?} does not line up with {slice:?}"
            );
        }
    }

    #[test]
    fn registering_makes_both_analyzers_reachable_by_name() {
        let index = Index::create_in_ram(crate::schema::build_schema().0);
        assert!(index.tokenizers().get(CODE_ANALYZER).is_none());
        assert!(index.tokenizers().get(PROSE_ANALYZER).is_none());
        register(&index);
        assert!(index.tokenizers().get(CODE_ANALYZER).is_some());
        assert!(index.tokenizers().get(PROSE_ANALYZER).is_some());
    }

    /// `(text, position)` for every token the `prose` analyzer emits.
    fn prose(text: &str) -> Vec<String> {
        let mut analyzer = prose_analyzer();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        stream.process(&mut |t: &Token| out.push(t.text.clone()));
        out
    }

    #[test]
    fn the_prose_analyzer_stems_and_the_code_analyzer_does_not() {
        assert_eq!(prose("compiling"), prose("compiled"));
        assert_eq!(
            prose("Indexes the sessions"),
            vec!["index", "the", "session"]
        );
        // The same words through `code` keep their endings, which is the point: a snippet
        // holding `parses` must not be indexed as `pars`.
        assert_eq!(texts("parses"), vec!["parses"]);
        assert_ne!(texts("compiling"), texts("compiled"));
    }

    #[test]
    fn the_prose_analyzer_splits_identifiers_too() {
        // Prose is full of identifiers nobody fenced or backticked, and every attachment,
        // `system` record and tool call lands in a prose field whole. Losing the split here
        // would lose `snippet` -> `SnippetGenerator` for all of them.
        assert_eq!(
            prose("open_or_create"),
            vec!["openorcr", "open", "or", "creat"]
        );
        assert_eq!(
            prose("SnippetGenerator"),
            vec!["snippetgener", "snippet", "generat"]
        );
        // Both spellings of one name still produce the same terms, stemmed.
        assert_eq!(prose("openOrCreate"), prose("open_or_create"));
        // A plain word is emitted once, and stemmed.
        assert_eq!(prose("Indexes"), vec!["index"]);
    }

    #[test]
    fn the_prose_analyzer_keeps_a_long_token_whole() {
        let hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(prose(hash), vec![hash.to_string()]);
        // ...but the stock 40-byte limit would not, which is why the limit is set here too.
        assert!(prose(&"a".repeat(300)).is_empty());
    }
}
