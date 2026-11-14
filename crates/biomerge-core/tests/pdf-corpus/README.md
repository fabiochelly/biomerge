# Synthetic PDF corpus

Test material for BioMerge's PDF extraction (`src/extract/pdf.rs`), read by `tests/pdf_corpus.rs`
and by the PDF tests of the application. Every text is made up for the corpus (`gen/texts.py`):
no private document, no external source.

## Layout

- `synth/<case>/` — one PDF per difficulty:
  - `doc.pdf`, the file;
  - `expected.txt`, what the page **shows**, one paragraph per line, in reading order, without
    page furniture (running headers and footers, page numbers, watermarks);
  - `case.json`, what the case tests (`note`, `signal`), extra `checks` on the output (`present`,
    `absent`, `count`, `any`; `sep` checks need the raw layout and are not read by the test), and
    for the robustness cases `password`, `expect_error`, `expect_scanned`, `partial_ok`.
- `pairs/<pair>/` — two versions of a document, `left.pdf` and `right.pdf`, their expected texts,
  and in `pair.json` the differences a reader should see (`expected_differences`, empty when the
  two only differ in layout: page break moved, header changed, other PDF producer).

Case families, by prefix: `c` hand-written PDF syntax where coordinates decide (order, spacing,
columns, drop cap, tagged order…); `h` HTML printed by Chromium (paragraphs over pages,
hyphenation, columns, tables, lists, notes, ligatures, accents, RTL, CJK…); `l` low-level traps
(missing or lying ToUnicode, Type3 fonts, overstruck accents, forms, watermark, blank pages,
scans with and without an OCR layer); `r` robustness (encrypted, owner password only, RC4, object
streams, linearized, broken xref, truncated, not a PDF, zero bytes, junk in a content stream).

## Regenerating

The generators are in `gen/`. They need Python 3, headless `chromium` (with Liberation fonts),
Ghostscript (`gs`), `qpdf`, and the URW AFM metrics under `/usr/share/libwmf/fonts/`. From this
directory:

```sh
python3 -I gen/raw_cases.py synth pairs                   # c*, l* cases and the raw pairs
python3 -I gen/html_cases.py synth pairs /tmp/big cases   # h* cases
python3 -I gen/html_cases.py synth pairs /tmp/big pairs   # the Chromium pairs
gen/robust_cases.sh synth                                 # r* cases, derived from h01
```

(`html_cases.py … big` makes a 2000-page document for speed tests; it is not kept here.)
Regenerated files differ in bytes from these (dates, producer versions), and may differ in
extraction: rerun the test and update its table.

## Reading the tolerances

`cargo test -p biomerge-core --test pdf_corpus -- --nocapture` prints one row per case:

- `cer` — character error rate: edit distance between the expected text and the extraction, both
  normalised the same way (ligatures, bullets, spaces), over the length of the expected text;
- `para` — share of expected paragraphs (three words or more) found whole on one output line,
  which is what the line diff needs;
- `checks` — passing `case.json` checks over those run;
- the verdict: `pass`, `expected-fail` with the reason, `REGRESSION` or `IMPROVED`.

A case not listed in the `KNOWN` table of `tests/pdf_corpus.rs` must be clean: `cer` ≤ 1 %, every
paragraph on one line, every check passing. A listed case is a known weakness (most are pdf_oxide's:
drop cap, tagged order, right-to-left scripts, overstruck accents, hyphens dropped inside lines),
recorded with its score today. Doing worse fails the test; doing clearly better (by 0.02, or one
check) fails it too, so that the table is updated and stays true.

The robustness cases are asserted exactly rather than scored: `r01` asks for its password
(`PasswordRequired`, then `WrongPassword`, then opens with `secret`), `r07` gives an error or
flagged pages and never garbage as text, `r08` and `r09` are clean errors, `l11` and `l14` are
reported scanned, `l12`'s OCR layer is text. The pairs are compared with BioMerge's `diff_lines`:
each expected difference must be found, and the blocks nothing explains are counted against the
`SPURIOUS_BLOCKS` table (all zero today).

To see what a score is made of:

```sh
PDF_CASE=h02_hyphenation cargo test -p biomerge-core --test pdf_corpus -- --ignored --nocapture show_one_case
```
