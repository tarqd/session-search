//! The `code` analyzer: one tokenizer that indexes identifiers the way people search for them.
//!
//! Transcripts are mostly code, paths and shell. The stock `default` analyzer indexes
//! `open_or_create` as three unrelated words and `SnippetGenerator` as one opaque blob, so
//! `create` misses the first and `snippet` misses the second. This module fixes both by
//! emitting, for every token, **the whole identifier plus each of its parts at the same
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
//! The base tokenizer is a `\w+` [`RegexTokenizer`] rather than [`SimpleTokenizer`], because
//! `SimpleTokenizer` splits on `_` before any filter can see it — an underscore-aware filter
//! sitting behind it would be dead code, and `OpenOrCreate` could never find `open_or_create`.
//! `\w+` is otherwise the same rule: a token is a run of word characters, everything else is a
//! separator, so `src/index.rs` still yields `src`, `index`, `rs`.
//!
//! [`RemoveLongFilter`] runs last with a 255-byte limit instead of the stock 40, so a sha256 or
//! a long generated identifier stays findable instead of being silently dropped.
//!
//! Changing anything here changes the terms on disk, so it changes the schema (the field's
//! tokenizer name is part of it) and `index::open_or_create` will discard and rebuild the
//! index on the next run.

use tantivy::Index;
use tantivy::tokenizer::{
    LowerCaser, RegexTokenizer, RemoveLongFilter, TextAnalyzer, Token, TokenFilter, TokenStream,
    Tokenizer,
};

/// The name the analyzer is registered under, and the name the schema refers to.
pub const CODE_ANALYZER: &str = "code";

/// Tokens longer than this are dropped. The stock limit is 40, which loses every sha256.
const MAX_TOKEN_BYTES: usize = 255;

/// A run of word characters — letters, digits and `_`.
const WORD: &str = r"\w+";

/// Build the `code` analyzer. Cheap enough to call per index; `TextAnalyzer` is `Clone`.
pub fn code_analyzer() -> TextAnalyzer {
    let base = RegexTokenizer::new(WORD).expect("the word-run pattern is a valid regex");
    TextAnalyzer::builder(base)
        .filter(SplitIdentifiers)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .build()
}

/// Register the `code` analyzer on `index`.
///
/// Must be called on **every** path that opens or creates an [`Index`], before any document is
/// added and before any query is parsed: both the writer and `QueryParser` look the analyzer up
/// by name, and a missing registration is an error at that point, not at open time.
pub fn register(index: &Index) {
    index.tokenizers().register(CODE_ANALYZER, code_analyzer());
}

/// A RAM index carrying the pinned schema **and** the `code` analyzer — the in-memory
/// counterpart of `index::open_or_create`, so no test can forget to register it and then fail
/// with `UnknownTokenizer` a dozen frames deep.
#[cfg(test)]
pub(crate) fn create_in_ram(schema: tantivy::schema::Schema) -> Index {
    let index = Index::create_in_ram(schema);
    register(&index);
    index
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
            token: Token::default(),
        }
    }
}

#[derive(Clone)]
pub struct SplitIdentifiersFilter<T> {
    inner: T,
    /// Tokens still owed for the current input token, in **reverse** emission order.
    pending: Vec<Token>,
    token: Token,
}

impl<T: Tokenizer> Tokenizer for SplitIdentifiersFilter<T> {
    type TokenStream<'a> = SplitIdentifiersStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.pending.clear();
        self.token.reset();
        SplitIdentifiersStream {
            tail: self.inner.token_stream(text),
            pending: &mut self.pending,
            token: &mut self.token,
        }
    }
}

pub struct SplitIdentifiersStream<'a, T> {
    tail: T,
    pending: &'a mut Vec<Token>,
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
            expand(self.tail.token(), self.pending);
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
/// (separators removed) followed by each part, all sharing `token.position`.
fn expand(token: &Token, out: &mut Vec<Token>) {
    let parts = parts_of(&token.text);

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

    // Parts first, reversed, so the whole identifier pops off the back first.
    for part in parts.iter().rev() {
        out.push(part_token(part));
    }
    let mut whole = String::with_capacity(token.text.len());
    for &(from, to) in &parts {
        whole.push_str(&token.text[from..to]);
    }
    out.push(Token {
        offset_from: token.offset_from,
        offset_to: token.offset_to,
        position: token.position,
        text: whole,
        position_length: 1,
    });
}

/// Byte ranges of the sub-parts of one token.
///
/// Splits on `_`, on a lower/digit -> upper transition (`parseTs`), on the last upper of an
/// upper run followed by a lower (`HTTPServer` -> `HTTP`, `Server`), and on any letter/digit
/// transition (`Ts2Ms` -> `Ts`, `2`, `Ms`).
fn parts_of(text: &str) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
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
            Some(p) if p != '_' => is_boundary(p, c, text[i..].chars().nth(1)),
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
    parts
}

/// Is there a part boundary *before* `cur`, which follows `prev` and precedes `next`?
fn is_boundary(prev: char, cur: char, next: Option<char>) -> bool {
    // camelCase / pascalCase: `eT` in `parseTs`.
    if !prev.is_uppercase() && prev.is_alphabetic() && cur.is_uppercase() {
        return true;
    }
    // An acronym meeting a word: the `S` of `HTTPServer`.
    if prev.is_uppercase() && cur.is_uppercase() && next.is_some_and(char::is_lowercase) {
        return true;
    }
    // Digits are their own part: `Ts2Ms`.
    prev.is_numeric() != cur.is_numeric()
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
    fn a_sha256_survives_the_length_filter_whole() {
        let hash = "a".repeat(64);
        assert_eq!(texts(&hash), vec![hash.clone()]);
        // A mixed hash keeps the whole form as its first term, so an exact paste still hits.
        let mixed = "0f3a".repeat(16);
        assert_eq!(texts(&mixed)[0], mixed);
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
    fn registering_makes_the_analyzer_reachable_by_name() {
        let index = Index::create_in_ram(crate::schema::build_schema().0);
        assert!(index.tokenizers().get(CODE_ANALYZER).is_none());
        register(&index);
        assert!(index.tokenizers().get(CODE_ANALYZER).is_some());
    }
}
