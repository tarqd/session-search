# Retrieval evaluation — recorded numbers

This file is the *record*. [`DESIGN.md`](DESIGN.md#retrieval-evaluation) explains why the harness
is built the way it is; this one holds what it printed, at a named commit, with the command that
reproduces it, so an issue or a pull request can cite a number without retyping it.

Every table below is **verbatim harness output**, copied out of `target/eval/*.md` without a
character changed — including the harness's own `##` headings, which is why they sit inside the
numbered sections rather than being renumbered to match. Nothing was transcribed by hand, and
that is checkable: run the command below and diff the artifacts against the blocks here. A table
that no longer matches means the numbers moved and this file is stale, which is the only failure
mode it has.

**Reproduce:**

```
cargo test --test eval -- --nocapture
```

Each run also writes `target/eval/report.md` (§1), `target/eval/ablation.md` (§2),
`target/eval/similar.md` (§3), `target/eval/facets.md` (§4), `target/eval/skeleton.md` (§5),
plus `corpus.md`, `hits.md` and `similar-hits.md`, which are the per-document listings a person
grading a new query reads.

**Taken at:** the review-fix commit on `claude/issues-21-23-26-workflow-1lglrp`, which is where these
numbers were last regenerated. `tests/fixtures/` is byte-identical to `cb810a1`, so the corpus and
the grades are unchanged; three things in the harness and one in `src/` moved, and each one is
visible in a table above:

- `tests/eval/metrics.rs` and `report.rs` gained the **`pinned`** column and its footnote —
  `nDCG@10` rows that are arithmetically fixed at `1.000`, the nDCG analogue of `ceiling`. It
  adds a column to §1–§3 and moves no digit of recall, MRR or nDCG.
- `tests/eval/similar.rs` gave the **text arm the same seed-turn exclusion the MoreLikeThis arm
  has**. Only the `text` column of §3's diff table moves, and it moves *up*: it had been charged
  for top-k slots spent on documents the protocol deletes from the graded set, which the
  similarity arm cannot return at all. §3's MoreLikeThis column is unchanged.
- `src/search.rs` replaced `MoreLikeThisQuery` with a deterministic reimplementation of the same
  query shape (see [DESIGN](DESIGN.md#similarity)). Every number in §3 is unchanged by it — the
  seeds in this corpus stay under the term cap, which is where the old implementation's
  non-determinism began — but before the fix these numbers were only stable by luck.

§1, §2 and §4's metrics are therefore identical to `cb810a1`'s.

**Corpus: 65 documents, 6 synthetic transcripts, 39 queries, 33 of them scored.** Read
[§6](#6-what-these-numbers-cannot-support) before quoting any of it. Two of the four scored
classes cannot move at all on the metric they look best on, and the reason is in the fixture
rather than in the ranker.

---

## 1. Baseline — issue #21

The shipped configuration (`context_text` on), scored per class at k = 10.

## Retrieval eval — context_text on

| class       | queries | scored | ceiling | recall@10 |       MRR |   nDCG@10 | pinned |
| ----------- | ------- | ------ | ------- | --------- | --------- | --------- | ------ |
| identifier  |       8 |      8 |       7 |     1.000 |     1.000 |     0.928 |      2 |
| boundary    |       8 |      8 |       2 |     0.913 |     1.000 |     0.774 |      1 |
| paraphrase  |       7 |      7 |       1 |     0.844 |     1.000 |     0.782 |      0 |
| filtered    |      10 |     10 |       7 |     1.000 |     1.000 |     0.981 |      5 |
| aggregation |       6 |      0 |       0 |         — |         — |         — |      0 |
| overall     |      39 |     33 |      17 |     0.946 |     1.000 |     0.876 |      8 |

6 of 39 queries are aggregation-shaped: recorded, never scored. Top-k is the wrong answer shape for them, and scoring one as a ranking would book a modelling mistake as a retrieval miss.

`ceiling` counts scored queries whose recall was forced to 1.000 by the fixture rather than earned by the ranker: the query matched fewer documents than the cutoff and every one of them is graded relevant, so `found == relevant == the whole match set` and no ranking could have scored it differently. 17 of 33 scored queries are in that state, marked † in the per-query table. Their recall cannot rise, cannot fall except by a document leaving the matched set entirely, and says nothing about ordering. A class whose `ceiling` equals its `scored` has a recall column that measures the corpus, not retrieval.

`pinned` counts scored queries whose nDCG@10 was forced to 1.000: the run returned exactly the graded set and every returned grade is equal, so DCG and IDCG are the same sum under any permutation and no reordering could change the number. 8 of 33 scored queries are in that state, marked ‡ in the per-query table. A class mean whose rows are mostly pinned has less headroom than the column suggests, and a delta on it is spread over the movable rows rather than over all of them.

And the class that has no retrieval score, because a ranking is the wrong answer shape for it — §4
is what it reports instead.

### What the baseline actually establishes

**The `ceiling` column is the headline, not the metrics beside it.** 17 of the 33 scored queries
have a recall that no ranker could have changed: the query matched fewer than ten documents, and
every document it matched is graded relevant, so `found == relevant == the whole match set` and
recall is `1.000` by arithmetic. In the identifier class that is 7 rows of 8; in the filtered class
7 of 10. `identifier recall@10 = 1.000` and `filtered recall@10 = 1.000` are therefore **not
findings about retrieval quality**. They say the matcher matches, which the analyzer invariant
assertions in `analyzer_invariants_hold_over_the_indexed_corpus` already say more directly and
more usefully.

**MRR is 1.000 in every scored class, and that is a dead instrument.** Across all 33 scored
queries, the rank-1 hit was graded relevant every time. A metric pinned at its maximum has no
resolution left: it can register a collapse and nothing else. It is worth keeping for exactly that
— it will stop being 1.000 the moment something breaks — but no change to ranking can ever be
argued *for* on the strength of it, because there is no room above.

**nDCG@10 is the only metric on this corpus with headroom in every class**: 0.928 / 0.774 / 0.782
/ 0.981. It is the number to watch and the number to argue from, and it is also the only one of
the three that the ceiling rows do not saturate — a ceiling row can still have its graded
documents in the wrong order.

---

## 2. Context header on/off — issue #23

The same corpus, the same fixture and the same arithmetic over an index built with the
`context_text` key removed from the JSON handed to Tantivy, so the two indexes differ in that
field's posting lists and in nothing else.

## Retrieval eval — context_text off

| class       | queries | scored | ceiling | recall@10 |       MRR |   nDCG@10 | pinned |
| ----------- | ------- | ------ | ------- | --------- | --------- | --------- | ------ |
| identifier  |       8 |      8 |       8 |     1.000 |     1.000 |     0.928 |      2 |
| boundary    |       8 |      8 |       1 |     0.875 |     1.000 |     0.767 |      1 |
| paraphrase  |       7 |      7 |       0 |     0.206 |     0.857 |     0.255 |      0 |
| filtered    |      10 |     10 |       9 |     1.000 |     1.000 |     0.977 |      6 |
| aggregation |       6 |      0 |       0 |         — |         — |         — |      0 |
| overall     |      39 |     33 |      18 |     0.801 |     0.970 |     0.761 |      9 |

`ceiling` counts scored queries whose recall was forced to 1.000 by the fixture rather than earned by the ranker: the query matched fewer documents than the cutoff and every one of them is graded relevant, so `found == relevant == the whole match set` and no ranking could have scored it differently. 18 of 33 scored queries are in that state, marked † in the per-query table. Their recall cannot rise, cannot fall except by a document leaving the matched set entirely, and says nothing about ordering. A class whose `ceiling` equals its `scored` has a recall column that measures the corpus, not retrieval.

`pinned` counts scored queries whose nDCG@10 was forced to 1.000: the run returned exactly the graded set and every returned grade is equal, so DCG and IDCG are the same sum under any permutation and no reordering could change the number. 9 of 33 scored queries are in that state, marked ‡ in the per-query table. A class mean whose rows are mostly pinned has less headroom than the column suggests, and a delta on it is spread over the movable rows rather than over all of them.

## Retrieval eval — context_text on

| class       | queries | scored | ceiling | recall@10 |       MRR |   nDCG@10 | pinned |
| ----------- | ------- | ------ | ------- | --------- | --------- | --------- | ------ |
| identifier  |       8 |      8 |       7 |     1.000 |     1.000 |     0.928 |      2 |
| boundary    |       8 |      8 |       2 |     0.913 |     1.000 |     0.774 |      1 |
| paraphrase  |       7 |      7 |       1 |     0.844 |     1.000 |     0.782 |      0 |
| filtered    |      10 |     10 |       7 |     1.000 |     1.000 |     0.981 |      5 |
| aggregation |       6 |      0 |       0 |         — |         — |         — |      0 |
| overall     |      39 |     33 |      17 |     0.946 |     1.000 |     0.876 |      8 |

`ceiling` counts scored queries whose recall was forced to 1.000 by the fixture rather than earned by the ranker: the query matched fewer documents than the cutoff and every one of them is graded relevant, so `found == relevant == the whole match set` and no ranking could have scored it differently. 17 of 33 scored queries are in that state, marked † in the per-query table. Their recall cannot rise, cannot fall except by a document leaving the matched set entirely, and says nothing about ordering. A class whose `ceiling` equals its `scored` has a recall column that measures the corpus, not retrieval.

`pinned` counts scored queries whose nDCG@10 was forced to 1.000: the run returned exactly the graded set and every returned grade is equal, so DCG and IDCG are the same sum under any permutation and no reordering could change the number. 8 of 33 scored queries are in that state, marked ‡ in the per-query table. A class mean whose rows are mostly pinned has less headroom than the column suggests, and a delta on it is spread over the movable rows rather than over all of them.

## context_text off → context_text on

| class       | metric    |    before |     after |     delta |
| ----------- | --------- | --------- | --------- | --------- |
| identifier  | recall    |     1.000 |     1.000 |     0.000 |
| identifier  | MRR       |     1.000 |     1.000 |     0.000 |
| identifier  | nDCG      |     0.928 |     0.928 |     0.000 |
| boundary    | recall    |     0.875 |     0.913 |    +0.039 |
| boundary    | MRR       |     1.000 |     1.000 |     0.000 |
| boundary    | nDCG      |     0.767 |     0.774 |    +0.007 |
| paraphrase  | recall    |     0.206 |     0.844 |    +0.638 |
| paraphrase  | MRR       |     0.857 |     1.000 |    +0.143 |
| paraphrase  | nDCG      |     0.255 |     0.782 |    +0.526 |
| filtered    | recall    |     1.000 |     1.000 |     0.000 |
| filtered    | MRR       |     1.000 |     1.000 |     0.000 |
| filtered    | nDCG      |     0.977 |     0.981 |    +0.004 |
| overall     | recall    |     0.801 |     0.946 |    +0.145 |
| overall     | MRR       |     0.970 |     1.000 |    +0.030 |
| overall     | nDCG      |     0.761 |     0.876 |    +0.114 |

Recall ceiling: 18 of 33 scored queries before and 17 of 33 after had their recall forced to 1.000 by the fixture — the query matched fewer documents than the cutoff and every one of them is graded relevant. A delta of 0.000 on a class made mostly of those rows means the class could not have moved, which is a different claim from the change being neutral.

### Per query, where it moved — context_text off → context_text on

| query                            | class       |  recall b |  recall a |    nDCG b |    nDCG a |
| -------------------------------- | ----------- | --------- | --------- | --------- | --------- |
| boundary-tokenizers-plural       | boundary    |     0.667 |     0.833 |     0.602 |     0.621 |
| boundary-indexes-plural          | boundary    |     0.875 |     0.875 |     0.568 |     0.585 |
| boundary-filter-prose-and-fence  | boundary    |     0.857 |     1.000 |     0.855 |     0.875 |
| para-build-fails                 | paraphrase  |     0.000 |     0.750 |     0.000 |     0.697 |
| para-retrieval-notes             | paraphrase  |     0.125 |     1.000 |     0.140 |     0.858 |
| para-split-identifiers           | paraphrase  |     0.286 |     0.857 |     0.268 |     0.695 |
| para-incremental-indexing        | paraphrase  |     0.167 |     0.667 |     0.468 |     0.886 |
| para-hash-rule-subagent          | paraphrase  |     0.167 |     1.000 |     0.191 |     0.863 |
| para-error-log                   | paraphrase  |     0.500 |     0.833 |     0.514 |     0.712 |
| para-release-artifact            | paraphrase  |     0.200 |     0.800 |     0.206 |     0.761 |
| filtered-project-other-tool      | filtered    |     1.000 |     1.000 |     0.963 |     1.000 |

22 further scored queries were unchanged on both recall and nDCG to three decimals and are omitted.

### The identifier class did not regress — with an important qualification

**It did not regress.** Class nDCG is `0.927802467` on both arms — equal to nine decimal places,
not merely equal at the three the table prints. No identifier query lost recall, and none lost
nDCG; the per-query deltas are `+0.000000` on all eight rows.

**But the identifier class was mostly incapable of regressing on recall, and the table should not
be read as if it were.** With the header off, the `ceiling` count for identifier is **8 of 8** —
every single identifier query had its recall forced to `1.000`. That column could not have fallen
unless a graded document left the matched set entirely. So "identifier recall: 1.000 → 1.000,
Δ 0.000" is a true statement about a number that had one reachable value.

The claim that *is* supported is the narrower one, and it is still worth making: **on the one
identifier metric with headroom — nDCG@10 — the context header changed nothing at all, to nine
decimal places.** `CONTEXT_BOOST = 0.3` did not reorder a single graded identifier document. One
identifier row and two filtered rows did leave the ceiling when the header was added
(identifier 8 → 7, filtered 9 → 7), which means the header widened those queries' matched sets past
the cutoff *without pushing any graded document out of the top ten* — the property the harness
asserts rather than prints.

### The paraphrase result is large enough to survive the corpus being small

Paraphrase recall `0.206 → 0.844` (+0.638) and nDCG `0.255 → 0.782` (+0.526), over 7 queries with
0 ceiling rows on either arm. This is not a three-point nDCG difference that a 65-document corpus
could not establish; it is a step change, and the per-query rows say why in a way an average
cannot — note that **eleven rows moved and twenty-two did not**, so the class means above are not
a broad drift, they are a handful of queries changing category:

### Per query, where it moved — context_text off → context_text on

| query                            | class       |  recall b |  recall a |    nDCG b |    nDCG a |
| -------------------------------- | ----------- | --------- | --------- | --------- | --------- |
| boundary-tokenizers-plural       | boundary    |     0.667 |     0.833 |     0.602 |     0.621 |
| boundary-indexes-plural          | boundary    |     0.875 |     0.875 |     0.568 |     0.585 |
| boundary-filter-prose-and-fence  | boundary    |     0.857 |     1.000 |     0.855 |     0.875 |
| para-build-fails                 | paraphrase  |     0.000 |     0.750 |     0.000 |     0.697 |
| para-retrieval-notes             | paraphrase  |     0.125 |     1.000 |     0.140 |     0.858 |
| para-split-identifiers           | paraphrase  |     0.286 |     0.857 |     0.268 |     0.695 |
| para-incremental-indexing        | paraphrase  |     0.167 |     0.667 |     0.468 |     0.886 |
| para-hash-rule-subagent          | paraphrase  |     0.167 |     1.000 |     0.191 |     0.863 |
| para-error-log                   | paraphrase  |     0.500 |     0.833 |     0.514 |     0.712 |
| para-release-artifact            | paraphrase  |     0.200 |     0.800 |     0.206 |     0.761 |
| filtered-project-other-tool      | filtered    |     1.000 |     1.000 |     0.963 |     1.000 |

22 further scored queries were unchanged on both recall and nDCG to three decimals and are omitted.

`para-build-fails` goes from **retrieving nothing** to retrieving three quarters of its graded set:
the words `build` and `fail` are in that session's title and in no body it has. That is a
mechanism demonstrated, not an effect size estimated — the honest reading is "the header makes
documents retrievable that were unreachable by any word a person would type", and *not* "the
header improves paraphrase recall by 64 points", which is a claim about a population this corpus
is not a sample of.

### The boundary and filtered movements support nothing

Read them off the per-query table above rather than off the class means. Boundary's `+0.039` recall
is **two** of eight rows moving (`boundary-tokenizers-plural`, `boundary-filter-prose-and-fence`);
its `+0.007` nDCG is those two plus `boundary-indexes-plural`, none of them by more than 0.021.
Filtered's `+0.004` nDCG is `filtered-project-other-tool` alone, at `+0.037`, and no filtered row
moved on recall at all. On class sizes of eight and ten, with no repeated trials and no variance
estimate of any kind, **these are not effects.** They are consistent with the header being neutral
for both classes, and nothing in this harness can distinguish "small real gain" from "one query
happened to reorder". Do not cite them.

### The third arm: how much of it is `sessions.json`

## context_text off → context_text without the session row

| class       | metric    |    before |     after |     delta |
| ----------- | --------- | --------- | --------- | --------- |
| identifier  | recall    |     1.000 |     1.000 |     0.000 |
| identifier  | MRR       |     1.000 |     1.000 |     0.000 |
| identifier  | nDCG      |     0.928 |     0.927 |    -0.001 |
| boundary    | recall    |     0.875 |     0.895 |    +0.021 |
| boundary    | MRR       |     1.000 |     1.000 |     0.000 |
| boundary    | nDCG      |     0.767 |     0.772 |    +0.005 |
| paraphrase  | recall    |     0.206 |     0.388 |    +0.182 |
| paraphrase  | MRR       |     0.857 |     0.857 |     0.000 |
| paraphrase  | nDCG      |     0.255 |     0.427 |    +0.172 |
| filtered    | recall    |     1.000 |     1.000 |     0.000 |
| filtered    | MRR       |     1.000 |     1.000 |     0.000 |
| filtered    | nDCG      |     0.977 |     0.977 |     0.000 |
| overall     | recall    |     0.801 |     0.845 |    +0.044 |
| overall     | MRR       |     0.970 |     0.970 |     0.000 |
| overall     | nDCG      |     0.761 |     0.799 |    +0.037 |

Recall ceiling: 18 of 33 scored queries before and 15 of 33 after had their recall forced to 1.000 by the fixture — the query matched fewer documents than the cutoff and every one of them is graded relevant. A delta of 0.000 on a class made mostly of those rows means the class could not have moved, which is a different claim from the change being neutral.

Paraphrase recall reaches `0.388` from the per-document header alone (project basename, branch,
turn prompt) against `0.844` with the session row. On this corpus roughly two thirds of the effect
needs `sessions.json`'s title and opening prompt. Same caveat as above about what "roughly two
thirds" is a measurement of: seven queries.

---

## 3. More like this — issue #26

The MoreLikeThis arm answers a different question from the fixture's own. Each row is seeded from the document that row graded highest, the query string is discarded, the row's filters are kept, and the seed's whole turn is removed from the graded set because `--similar-to` excludes it by default. 27 of 39 fixture rows are scored here: 6 were dropped for having no graded document outside the seed's own turn, and the aggregation-shaped rows are not ranked answers at all.

Read `recall` and ignore everything else: MRR and nDCG are reported for symmetry with the baseline table, but a similarity search has no notion of "the answer" to rank first.

## Retrieval eval — more like this, seeded from the top-graded document

| class       | queries | scored | ceiling | recall@10 |       MRR |   nDCG@10 | pinned |
| ----------- | ------- | ------ | ------- | --------- | --------- | --------- | ------ |
| identifier  |       6 |      6 |       0 |     0.361 |     0.333 |     0.210 |      0 |
| boundary    |       7 |      7 |       0 |     0.570 |     0.571 |     0.526 |      0 |
| paraphrase  |       6 |      6 |       0 |     0.589 |     0.297 |     0.334 |      0 |
| filtered    |       8 |      8 |       1 |     0.875 |     0.547 |     0.643 |      1 |
| overall     |      27 |     27 |       1 |     0.618 |     0.450 |     0.448 |      1 |

0 of 27 queries are aggregation-shaped: recorded, never scored. Top-k is the wrong answer shape for them, and scoring one as a ranking would book a modelling mistake as a retrieval miss.

`ceiling` counts scored queries whose recall was forced to 1.000 by the fixture rather than earned by the ranker: the query matched fewer documents than the cutoff and every one of them is graded relevant, so `found == relevant == the whole match set` and no ranking could have scored it differently. 1 of 27 scored queries are in that state, marked † in the per-query table. Their recall cannot rise, cannot fall except by a document leaving the matched set entirely, and says nothing about ordering. A class whose `ceiling` equals its `scored` has a recall column that measures the corpus, not retrieval.

`pinned` counts scored queries whose nDCG@10 was forced to 1.000: the run returned exactly the graded set and every returned grade is equal, so DCG and IDCG are the same sum under any permutation and no reordering could change the number. 1 of 27 scored queries are in that state, marked ‡ in the per-query table. A class mean whose rows are mostly pinned has less headroom than the column suggests, and a delta on it is spread over the movable rows rather than over all of them.

## text query, seed's turn ungraded → more like this, seeded from the top-graded document

| class       | metric    |    before |     after |     delta |
| ----------- | --------- | --------- | --------- | --------- |
| identifier  | recall    |     1.000 |     0.361 |    -0.639 |
| identifier  | MRR       |     1.000 |     0.333 |    -0.667 |
| identifier  | nDCG      |     0.903 |     0.210 |    -0.693 |
| boundary    | recall    |     0.914 |     0.570 |    -0.344 |
| boundary    | MRR       |     1.000 |     0.571 |    -0.429 |
| boundary    | nDCG      |     0.793 |     0.526 |    -0.267 |
| paraphrase  | recall    |     1.000 |     0.589 |    -0.411 |
| paraphrase  | MRR       |     0.889 |     0.297 |    -0.592 |
| paraphrase  | nDCG      |     0.865 |     0.334 |    -0.531 |
| filtered    | recall    |     1.000 |     0.875 |    -0.125 |
| filtered    | MRR       |     1.000 |     0.547 |    -0.453 |
| filtered    | nDCG      |     1.000 |     0.643 |    -0.357 |
| overall     | recall    |     0.978 |     0.618 |    -0.360 |
| overall     | MRR       |     0.975 |     0.450 |    -0.525 |
| overall     | nDCG      |     0.895 |     0.448 |    -0.447 |

Recall ceiling: 13 of 27 scored queries before and 1 of 27 after had their recall forced to 1.000 by the fixture — the query matched fewer documents than the cutoff and every one of them is graded relevant. A delta of 0.000 on a class made mostly of those rows means the class could not have moved, which is a different claim from the change being neutral.

Dropped from this arm: ident-sha256-pasted-whole, ident-release-flag-phrase, boundary-fenced-identifier-only, para-hash-rule-subagent, filtered-sidechains-only, filtered-agent-type.

### Three of the four classes are a category error, not a result

The `-0.639` on identifier recall is not MoreLikeThis performing badly. It is the harness asking
a similarity engine a question that has no similarity content, and the number should be read as
evidence that the *measurement* does not transfer rather than that the *feature* is weak.

- **identifier (0.361) and boundary (0.570) are meaningless as quality numbers.** Both classes
  exist to pin analyzer behaviour on a *typed query*, and this arm has no typed query — the query
  string is discarded by construction. What they measure instead is "having found one document
  containing `open_or_create`, do the others happen to be nearby", which nothing in the product
  claims and nobody would design for. Two rows score a flat `0.000`
  (`ident-iserror-no-underscore`, `ident-uuid-first-chunk`); asking "what is similar to the
  document containing this uuid" is not a question, and `0.000` is the correct answer to it.
- **filtered (0.875) is the one class that transfers cleanly**, and it is the number worth having:
  it says the structured filters still AND on top of the similarity clause, which is a real
  property of `--similar-to` and the one the feature would be broken without. Note its `ceiling`
  of 1 — `filtered-errors-only-exit-101` matched two documents and both are graded, so its 1.000
  is arithmetic — and read 0.875 as "the filters compose", not as a similarity score.
- **paraphrase (0.589) is the only fair test in the table**, because it is the class where the
  query words are deliberately not the transcript's words, which is the situation similarity is
  for. It loses 0.411 recall to a text query that has the `context_text` header working for it —
  and that text arm now searches the *same* candidate set, with the seed's turn removed from its
  hits as well as from the graded set. Before that symmetry it scored 0.706 here, understated,
  because it was spending top-k slots on documents the protocol had already deleted from the
  graded set while the MoreLikeThis arm could not return them at all.
- **MRR (0.450) and nDCG (0.448) should not be quoted at all.** A similarity search has no notion
  of "the answer" belonging at rank 1; those columns are printed for shape-compatibility with the
  baseline table and mean nothing here.

### The number to beat, and the number that is really being reported

For a future neural reranker: **paraphrase recall 0.589, filtered recall 0.875, overall recall
0.618 at k = 10**, on this fixture and this protocol. Any proposal that changes the protocol — the
seed choice, the source-turn exclusion, the narrowing of the graded set — has to say so, because
the protocol is doing at least as much work as the ranker.

### One row returns nothing, and one tuning constant explains most of the table

`filtered-lang-python` retrieves **zero documents**: a `--lang python` filter ANDed with a
similarity clause whose seed shares no surviving term with either python fence. That is the
empty-`BooleanQuery` outcome arriving as an honest `0.000` rather than as an error, which is what
the harness's `mlt.overall.recall > 0.0` assertion exists to catch if it ever becomes the whole
table.

More importantly: at 65 documents, `SIMILAR_MIN_DOC_FREQUENCY = 3` is the *only* term filter doing
any work. `SIMILAR_MAX_DOC_FREQUENCY_FLOOR = 50` means nothing on this corpus is ever cut for being
too common, so the upper document-frequency bound — the parameter that does the most work on a real
index — is entirely inert in every number in §3. A term must appear in 3 of 65 documents (4.6%) to
survive, which is punishing, and it is exactly what zeroes the uuid row. **These numbers
characterise a corpus-size regime the feature will never actually run in.**

---

## 4. The aggregation class — what it reports instead of a retrieval score

Issue #21 asks for numbers per class, and one class has no retrieval score by design. This is what
it has instead.

| query | facet field | docs matched | buckets returned | expected buckets, with the counts that came back |
| --- | --- | --- | --- | --- |
| `agg-which-tools-errored` | `tool_name` | 9 | 4 | `Bash` 4, `Read` 2, `Grep` 2, `Edit` 1 |
| `agg-which-projects-errored` | `project` | 9 | 1 | `/home/user/code/other-tool` 9 |
| `agg-which-sessions-ran-cargo` | `session_id` | 18 | 4 | `eval-tokenizer` 2, `eval-buildfail` 13, `b20208d8-fbdb-5918-ba69-d203de6ed6dc` 2 |
| `agg-which-programs-ran` | `bash_cmd.program` | 13 | 3 | `grep` 1, `rg` 1 |
| `agg-which-languages-were-quoted` | `code_lang` | 20 | 1 | `rust` 2 |
| `agg-which-files-were-missing` | `tool_input.file_path` | 9 | 3 | `/home/user/code/other-tool/logs/missing.txt` 1, `/home/user/code/other-tool/logs/archive.txt` 1 |

### Two of these six rows have a degenerate answer

`agg-which-projects-errored` returns **one** bucket over 9 matched documents, and
`agg-which-languages-were-quoted` returns **one** bucket over 20. The assertion each row carries —
"the named buckets come back non-empty" — passes, but a distribution with a single bucket does not
exercise the claim the rows were written to make. The `code_lang` row's own note says the point is
that "one answer holding a rust fence and a bash fence is one document and two bucket increments";
the corpus never produces that document, so the multi-valued behaviour is asserted in prose and not
in the fixture. **That is a corpus gap, not a passing test.** Fixing it means adding transcripts,
which changes every number in §1–§3, so it is recorded here rather than done alongside these
numbers.

---

## 5. Turn skeletons — issue #25

Not a retrieval metric. This one measures **size**: what a hit's turn context costs as documents
against what it costs as a skeleton, in the unit a caller actually pays in — the bytes of the
JSON the response carries. `--context turn --json` sends the turn's documents (`format::doc_json`
each); `--context skeleton --json` sends the skeleton object instead. Both are rendered by the
functions the CLI renders them with, so the ratio cannot drift away from the product.

The corpus is wider here than in §1–§4: the two `real_*_slice.jsonl` captures come first,
because they are what makes this "on a real corpus", and the eval fixtures follow.

```
| transcript                                 |     turns |      docs |     ctx B |    skel B |  skel % |
|--------------------------------------------|-----------|-----------|-----------|-----------|---------|
| real_main_slice.jsonl                      |         1 |         9 |     14371 |      1596 |    11.1 |
| real_sidechain_slice.jsonl                 |         1 |        11 |     19992 |      1642 |     8.2 |
| b20208d8-fbdb-5918-ba69-d203de6ed6dc.jsonl |         3 |        12 |     13560 |      1078 |     7.9 |
| eval-buildfail.jsonl                       |         3 |        13 |     12429 |      1096 |     8.8 |
| eval-facets.jsonl                          |         2 |        13 |     12832 |      1261 |     9.8 |
| eval-notes.jsonl                           |         2 |         9 |      8419 |       954 |    11.3 |
| eval-tokenizer.jsonl                       |         4 |        12 |     12360 |      1259 |    10.2 |
| ALL                                        |        16 |        79 |     93963 |      8886 |     9.5 |

mean turn: 5872 B of context, 555 B of skeleton
worst turn: 19992 B of context, 1642 B of skeleton
```

Bytes, not tokens: a token count needs a tokenizer this crate does not ship and would not agree
with whichever model reads the output. Divide by ~4 for an English-and-JSON estimate — the mean
turn goes from roughly 1500 tokens to 140, and the worst turn in the corpus from roughly 5000
to 410.

**What the shape of the table says.** The saving is not an average trick: every file lands
between 7.9% and 11.3%, and the *worst* turn — `real_sidechain_slice.jsonl`, which is a whole
subagent transcript, because rule 3 of the Turns section makes a sidechain one turn — is also
the one where the byte cap does the most work. The consistency is the mechanism showing through:
what a skeleton drops is `tool_output`, and `tool_output` is the overwhelming majority of the
bytes in every transcript here.

**What it does not measure.** The anchor document of a hit is still sent in full, with its own
`body` and `tool_output`; this is the cost of a hit's *context*, not of a whole response. And
the byte cap (`format::SKELETON_BUDGET`, 1600) is not reached by the mean turn at all, so this
table cannot tell you what the cap does to a turn with two hundred tool calls in it — only that
such a turn is bounded, which is what the cap is for.

---

## 6. What these numbers cannot support

Stated flatly, because the tables above will be quoted and the caveats will not travel with them
unless they are this blunt.

1. **Two of four scored classes have a saturated recall column.** identifier (7/8 ceiling rows)
   and filtered (7/10) print `recall@10 = 1.000` because the fixture graded the entire match set,
   not because retrieval is perfect. Any claim of the form "identifier retrieval is solved" or
   "the change was neutral for filtered" is unfalsifiable on this fixture.
2. **MRR is dead at 1.000.** It can detect a break. It cannot support any argument for a change.
3. **No variance, no trials, no confidence interval anywhere.** Every number is a single
   deterministic run over one fixed corpus. A delta under roughly 0.05 on a class of 7-10 queries
   is one query moving and should be treated as noise. That rules out citing boundary `+0.007`,
   filtered `+0.004`, or the identifier third-arm `-0.001` as anything at all.
4. **65 documents is the wrong size for BM25.** Length normalisation is relative to `avgdl` over
   the whole index and IDF is relative to corpus document frequency; both are dominated here by
   which six transcripts happen to be checked in. `CONTEXT_BOOST = 0.3` was chosen against a
   65-document `avgdl`, and this harness cannot tell you whether it is right at 65,000.
5. **Precision is not measured at all.** Only graded documents count. A query returning ten hits
   of which three are graded scores identically to one returning exactly those three. The
   `ident-source-path` row is a known live instance: a path is three ANDed terms rather than a
   phrase, so `src/lib.rs` beside the word `index` also matches, and the harness charges nothing
   for it.
6. **Only planted mechanisms are exercised.** Every behaviour this corpus tests is one somebody
   deliberately wrote into it. A retrieval failure nobody thought of is not in these tables, and
   that is the standing argument for adding rows written against redacted real slices — the
   fixture's `provenance.real_slices` is empty today and every row says `synthesised`.
7. **§3 says almost nothing about the similarity tuning.** See the last paragraph of §3: the
   parameter that governs `--similar-to` on a real index is inert at this corpus size.
8. **§5 is sixteen turns.** The skeleton ratio is consistent across all sixteen and across two
   real captures, which is why it is quotable as an order of magnitude and not as `9.5%`. A
   corpus with a forty-call turn in it would move the mean and would be the first real test of
   the byte cap; there is no such turn in `tests/fixtures/`.

The one claim in this file that the numbers do carry on their own weight is the paraphrase result
in §2 — and even that is a demonstrated mechanism (documents that were unreachable become
reachable) rather than an estimated effect size.
