//! The synthetic PDF corpus of `tests/pdf-corpus` (its README tells how it is made and read):
//! every case extracted and scored against its expected text, the robustness cases asserted
//! exactly, and the pairs compared with the line diff.
//!
//! Scores are on texts normalised the same way on both sides (ligatures, bullets, spaces), so
//! that only what the reader and the post-processing got wrong counts:
//! - `cer`: character error rate, edit distance over the length of the expected text;
//! - `para`: share of expected paragraphs (three words or more) found whole on one line, which
//!   is what the diff needs;
//! - `checks`: the `present`, `absent`, `count` and `any` checks of `case.json` that pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use biomerge_core::diff::{DiffOptions, diff_lines};
use biomerge_core::extract::{self, DocFormat, ExtractError, ExtractOptions, Extracted};
use serde_json::Value;

/// What a case may score at worst. Cases not listed must be clean: `cer` ≤ 1 %, every
/// paragraph on one line, every check passing. A listed case is a known weakness, recorded with
/// its score today and why: a regression fails the test, and so does a clear improvement, so
/// that the table stays true.
struct Tolerance {
    case: &'static str,
    cer: f64,
    para: f64,
    failed_checks: usize,
    why: &'static str,
}

const fn known(case: &'static str, cer: f64, para: f64, failed_checks: usize, why: &'static str) -> Tolerance {
    Tolerance { case, cer, para, failed_checks, why }
}

const KNOWN: &[Tolerance] = &[
    known("c09_columns_pages", 0.0257, 1.0, 0, "one page: its running header has nothing to repeat against and stays"),
    known("c10_drop_cap", 0.0138, 0.0, 1, "the drop cap comes out apart from its word (pdf_oxide)"),
    known("c14_tagged_order", 0.7726, 1.0, 0, "the structure tree's reading order is ignored, content order wins (pdf_oxide)"),
    known("h02_hyphenation", 0.0019, 0.625, 0, "pdf_oxide drops some hyphens inside lines: portemonnaie, arcen-ciel"),
    known("h03_two_columns", 0.0005, 0.9, 0, "pdf_oxide drops a hyphen inside a line: aprèsmidi"),
    known("h04_three_columns", 0.0860, 0.8, 0, "narrow columns: pdf_oxide interleaves a few lines of neighbouring columns"),
    known("h07_table_long", 0.0047, 0.992, 0, "the table header repeated on every page is dropped as a running line"),
    known("h14_rtl_mixed", 0.1655, 0.0, 2, "Arabic and Hebrew come out in visual order (pdf_oxide)"),
    known("h18_sup_sub", 0.0147, 0.0, 1, "a subscript comes out as a Unicode subscript digit: H₂O"),
    known("l02_broken_tounicode", 0.4125, 0.5, 0, "a lying character map shifts the letters of half the page, under the illegibility threshold"),
    known("l04_accent_overstrike", 0.0588, 0.0, 1, "an accent drawn over its letter comes out apart: cafe ´"),
    known("l06_forms_annots", 0.0, 0.5, 0, "a label ending with ':' is never joined with its value, by rule"),
    known("l07_watermark", 0.0203, 1.0, 0, "the CONFIDENTIEL watermark is kept as text"),
    known("r10_bad_content_stream", 0.1233, 0.5, 0, "junk operators cut the first page's text (pdf_oxide)"),
];

/// How far a score may move from the table before the test asks for the table to change.
const SLACK: f64 = 0.02;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pdf-corpus")
}

fn case_dirs(group: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(corpus().join(group))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn name(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The scoring normalisation of both sides: what the corpus's expected texts leave out.
fn score_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            let mut line: String = line
                .chars()
                .filter(|&c| c != '\u{ad}')
                .map(|c| match c {
                    '|' | '\t' => ' ',
                    '\u{2010}' | '\u{2011}' => '-',
                    c => c,
                })
                .collect();
            for (ligature, letters) in [("ﬀ", "ff"), ("ﬁ", "fi"), ("ﬂ", "fl"), ("ﬃ", "ffi"), ("ﬄ", "ffl")] {
                line = line.replace(ligature, letters);
            }
            let trimmed = line.trim_start();
            let mut chars = trimmed.chars();
            if let (Some(first), Some(' ')) = (chars.next(), chars.next())
                && "•◦▪‣·∙○■□➢►-*+".contains(first)
            {
                line = trimmed[first.len_utf8() + 1..].to_owned();
            }
            line.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|line| !line.is_empty())
        .collect()
}

fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(x != y)).min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn cer(expected: &[String], output: &[String]) -> f64 {
    let expected: Vec<char> = expected.join(" ").chars().collect();
    let output: Vec<char> = output.join(" ").chars().collect();
    levenshtein(&expected, &output) as f64 / expected.len().max(1) as f64
}

fn para(expected: &[String], output: &[String]) -> f64 {
    let paragraphs: Vec<&String> = expected.iter().filter(|line| line.split_whitespace().count() >= 3).collect();
    if paragraphs.is_empty() {
        return 1.0;
    }
    let whole = paragraphs.iter().filter(|paragraph| output.iter().any(|line| line.contains(paragraph.as_str()))).count();
    whole as f64 / paragraphs.len() as f64
}

/// The checks of `case.json` that can be read on the output alone, and how many fail.
fn checks(case: &Value, output: &[String]) -> (usize, usize) {
    let flat = output.join(" ");
    let mut run = 0;
    let mut failed = 0;
    for check in case["checks"].as_array().into_iter().flatten() {
        let text = check["text"].as_str().unwrap_or("");
        let ok = match check["type"].as_str().unwrap() {
            "present" => flat.contains(text),
            "present_ci" => flat.to_lowercase().contains(&text.to_lowercase()),
            "absent" => !flat.contains(text),
            "count" => flat.matches(text).count() as u64 == check["n"].as_u64().unwrap(),
            "any" => check["texts"].as_array().unwrap().iter().any(|t| flat.contains(t.as_str().unwrap())),
            _ => continue,
        };
        run += 1;
        failed += usize::from(!ok);
    }
    (run, failed)
}

fn extract_case(dir: &Path, password: Option<&str>) -> Result<Extracted, ExtractError> {
    extract::extract_with_password(&dir.join("doc.pdf"), &ExtractOptions::default(), password)
}

/// The robustness and scan cases, asserted exactly rather than scored.
const EXACT: [&str; 6] = ["r01_encrypted_user", "r07_truncated", "r08_not_a_pdf", "r09_zero_bytes", "l11_scan_image_only", "l14_gs_scan_multipage"];

#[test]
fn every_case_of_the_corpus_scores_within_its_tolerance() {
    let known: HashMap<&str, &Tolerance> = KNOWN.iter().map(|t| (t.case, t)).collect();
    let mut report = vec![format!("{:<28} {:>7} {:>6} {:>7}  verdict", "case", "cer", "para", "checks")];
    let mut seen = Vec::new();
    let mut problems = Vec::new();
    for dir in case_dirs("synth") {
        let case = name(&dir);
        if EXACT.contains(&case.as_str()) {
            continue;
        }
        let info = read_json(&dir.join("case.json"));
        let password = info["password"].as_str();
        let extracted = extract_case(&dir, password).unwrap_or_else(|error| panic!("{case}: {error}"));
        assert_eq!(extracted.format, DocFormat::Pdf);
        let expected = score_lines(&std::fs::read_to_string(dir.join("expected.txt")).unwrap());
        let output = score_lines(&extracted.text);
        let (cer, para) = if expected.is_empty() {
            (if output.is_empty() { 0.0 } else { 1.0 }, 1.0)
        } else {
            (cer(&expected, &output), para(&expected, &output))
        };
        let (run, failed) = checks(&info, &output);
        let limit = known.get(case.as_str());
        seen.push(case.clone());
        // The table is rounded to four and three decimals.
        let (max_cer, min_para, max_failed) = limit.map_or((0.01, 1.0, 0), |t| (t.cer + 0.0001, t.para - 0.001, t.failed_checks));
        let mut verdict = if limit.is_some() { "expected-fail".to_owned() } else { "pass".to_owned() };
        if cer > max_cer + 1e-9 || para < min_para - 1e-9 || failed > max_failed {
            verdict = "REGRESSION".to_owned();
            problems.push(format!("{case}: cer {cer:.4} (≤ {max_cer}), para {para:.3} (≥ {min_para}), failed checks {failed} (≤ {max_failed})"));
        } else if let Some(t) = limit
            && (cer < t.cer - SLACK || para > t.para + SLACK || failed < t.failed_checks)
        {
            verdict = "IMPROVED".to_owned();
            problems.push(format!("{case}: better than the table (cer {cer:.4}, para {para:.3}, failed checks {failed}): update it"));
        }
        let why = limit.map_or("", |t| t.why);
        report.push(format!("{case:<28} {cer:>7.4} {para:>6.3} {:>7}  {verdict} {why}", format!("{}/{run}", run - failed)));
    }
    println!("{}", report.join("\n"));
    for t in KNOWN.iter().filter(|t| !seen.iter().any(|case| case == t.case)) {
        problems.push(format!("{}: in the table but not in the corpus", t.case));
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn an_encrypted_pdf_asks_for_its_password_and_opens_with_it() {
    let dir = corpus().join("synth/r01_encrypted_user");
    assert!(matches!(extract_case(&dir, None), Err(ExtractError::PasswordRequired(_))));
    assert!(matches!(extract_case(&dir, Some("faux")), Err(ExtractError::WrongPassword(_))));
    let opened = extract_case(&dir, Some("secret")).unwrap();
    let expected = score_lines(&std::fs::read_to_string(dir.join("expected.txt")).unwrap());
    assert!(cer(&expected, &score_lines(&opened.text)) < 0.01);
    // Encrypted with an empty user password: no question asked.
    for case in ["r02_owner_only", "r03_rc4_legacy"] {
        let opened = extract_case(&corpus().join("synth").join(case), None).unwrap();
        assert!(opened.text.contains("Le comité de direction"), "{case}");
    }
}

#[test]
fn a_truncated_pdf_gives_an_error_or_flagged_pages_never_garbage_as_text() {
    let dir = corpus().join("synth/r07_truncated");
    let expected = std::fs::read_to_string(dir.join("expected.txt")).unwrap();
    match extract_case(&dir, None) {
        Err(ExtractError::Pdf { .. }) => {}
        Err(error) => panic!("unexpected error: {error}"),
        Ok(extracted) => {
            assert!(extracted.facts.illegible_pages + extracted.facts.unread_pages > 0, "{:?}", extracted.facts);
            // What is shown as text is text of the document; the rest is a placeholder.
            for line in extracted.text.lines() {
                assert!(line.starts_with("[page illisible") || expected.contains(line), "garbage shown as text: {line}");
            }
        }
    }
}

#[test]
fn what_is_not_a_pdf_is_refused_cleanly() {
    let not_pdf = extract_case(&corpus().join("synth/r08_not_a_pdf"), None).unwrap_err();
    assert!(matches!(not_pdf, ExtractError::NotPdf(_)), "{not_pdf}");
    assert!(not_pdf.to_string().ends_with("ce n'est pas un PDF (en-tête %PDF- absent)"));
    let empty = extract_case(&corpus().join("synth/r09_zero_bytes"), None).unwrap_err();
    assert!(matches!(empty, ExtractError::Empty(_)), "{empty}");
}

#[test]
fn a_scanned_pdf_says_so_and_has_no_text_to_compare() {
    for (case, pages) in [("l11_scan_image_only", 1), ("l14_gs_scan_multipage", 3)] {
        let extracted = extract_case(&corpus().join("synth").join(case), None).unwrap();
        assert_eq!(extracted.text, "", "{case}");
        assert_eq!((extracted.facts.pages, extracted.facts.scanned_pages), (pages, pages), "{case}");
        let notices = extracted.facts.notices(&ExtractOptions::default());
        assert_eq!(notices, ["document scanné : texte non comparé"], "{case}");
    }
    // An OCR layer under the image is text: compared.
    let ocr = extract_case(&corpus().join("synth/l12_ocr_invisible_layer"), None).unwrap();
    assert_eq!(ocr.facts.scanned_pages, 0);
    assert!(ocr.facts.notices(&ExtractOptions::default()).is_empty());
    // Blank pages are only counted.
    let blank = extract_case(&corpus().join("synth/l09_blank_middle_page"), None).unwrap();
    assert_eq!((blank.facts.pages_without_text, blank.facts.scanned_pages), (1, 0));
    assert_eq!(blank.facts.notices(&ExtractOptions::default()), ["1 page sans texte"]);
}

/// Blocks of the line diff that no expected difference explains, per pair: what the extraction
/// still gets wrong, documented here so that any change shows.
const SPURIOUS_BLOCKS: [(&str, usize); 7] = [
    ("p1_word_changed", 0),
    ("p2_para_inserted", 0),
    ("p3_table_cell", 0),
    ("p4_page_break_moved", 0),
    ("p5_header_changed", 0),
    ("p6_same_visual_other_code", 0),
    ("p7_scrambled_word_changed", 0),
];

#[test]
fn the_pairs_differ_by_their_expected_differences_only() {
    let spurious: HashMap<&str, usize> = SPURIOUS_BLOCKS.into_iter().collect();
    let mut problems = Vec::new();
    for dir in case_dirs("pairs") {
        let pair = name(&dir);
        let info = read_json(&dir.join("pair.json"));
        let side = |file: &str| {
            let extracted = extract::extract(&dir.join(file), &ExtractOptions::default()).unwrap();
            extracted.text.lines().map(str::to_owned).collect::<Vec<_>>()
        };
        let (left, right) = (side("left.pdf"), side("right.pdf"));
        let diff = diff_lines(&left, &right, DiffOptions::default());
        // An expected difference is found when one block holds its removed words on the left
        // and its added words on the right.
        let mut explained = std::collections::HashSet::new();
        let mut found = 0;
        let expected = info["expected_differences"].as_array().unwrap();
        for difference in expected {
            let words = |key: &str| difference[key].as_str().unwrap_or("").split_whitespace().map(str::to_owned).collect::<std::collections::HashSet<_>>();
            let (old, new) = (words("left"), words("right"));
            let added: Vec<&String> = new.difference(&old).collect();
            let removed: Vec<&String> = old.difference(&new).collect();
            let hit = diff.blocks.iter().position(|block| {
                let text = |lines: &[String], range: &std::ops::Range<u32>| lines[range.start as usize..range.end as usize].join(" ");
                let (l, r) = (text(&left, &block.left), text(&right, &block.right));
                let (l, r): (Vec<&str>, Vec<&str>) = (l.split_whitespace().collect(), r.split_whitespace().collect());
                added.iter().all(|w| r.contains(&w.as_str())) && removed.iter().all(|w| l.contains(&w.as_str()))
            });
            if let Some(block) = hit {
                found += 1;
                explained.insert(block);
            }
        }
        let extra = diff.blocks.len() - explained.len();
        println!("{pair:<28} found {found}/{} spurious {extra}", expected.len());
        if found < expected.len() {
            problems.push(format!("{pair}: {found} of {} expected differences found", expected.len()));
        }
        if extra != spurious[pair.as_str()] {
            problems.push(format!("{pair}: {extra} spurious blocks, the table says {}", spurious[pair.as_str()]));
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

/// `PDF_CASE=h02_hyphenation cargo test -p biomerge-core --test pdf_corpus -- --ignored
/// --nocapture`: the extracted text of one case beside its expected text, to see what a score
/// is made of.
#[test]
#[ignore]
fn show_one_case() {
    let case = std::env::var("PDF_CASE").expect("PDF_CASE names a case of synth/");
    let dir = corpus().join("synth").join(&case);
    let info = read_json(&dir.join("case.json"));
    match extract_case(&dir, info["password"].as_str()) {
        Ok(extracted) => {
            println!("--- extracted ({:?}, pages {:?})\n{}", extracted.facts, extracted.page_starts, extracted.text);
        }
        Err(error) => println!("--- error: {error}"),
    }
    println!("--- expected\n{}", std::fs::read_to_string(dir.join("expected.txt")).unwrap());
}
