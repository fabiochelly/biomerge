use std::fs;

use super::*;
use crate::diff::{IGNORE_PRESETS, IgnoreRule};
use crate::textedit::{self, TextPos};

fn doc(text: &str) -> Document {
    Document::from_text(text)
}

fn merge_with(base: &str, left: &str, right: &str, options: MergeOptions) -> Merge {
    let merge = Merge::new(doc(base), doc(left), doc(right), options).unwrap();
    check(&merge);
    merge
}

fn merge(base: &str, left: &str, right: &str) -> Merge {
    merge_with(base, left, right, MergeOptions::default())
}

fn text(doc: &Document) -> String {
    String::from_utf8(doc.encode().unwrap()).unwrap()
}

fn marked(merge: &Merge) -> String {
    let mut out = doc("");
    out.lines = Arc::new(merge.marked_lines());
    text(&out)
}

fn kinds(merge: &Merge) -> Vec<ChunkKind> {
    merge.chunks().iter().map(|chunk| chunk.kind).collect()
}

fn whitespace(whitespace: Whitespace) -> MergeOptions {
    MergeOptions { diff: DiffOptions { whitespace, ignore_case: false }, ..MergeOptions::default() }
}

fn lines_of(doc: &Document, range: &Range<u32>) -> Vec<String> {
    doc.lines[range.start as usize..range.end as usize].iter().map(|line| line.text.clone()).collect()
}

/// In each text the chunks cover every line once and in order, the result ranges match the
/// result document, and each kind says the truth about its texts (exact options only).
fn check(merge: &Merge) {
    let chunks = merge.chunks();
    assert!(!chunks.is_empty());
    for version in [Version::Base, Version::Left, Version::Right, Version::Result] {
        let mut next = 0;
        for chunk in chunks {
            let range = chunk.range(version);
            assert_eq!(range.start, next, "{version:?} {chunks:#?}");
            assert!(range.start <= range.end);
            next = range.end;
        }
        assert_eq!(next as usize, merge.document(version).len(), "{version:?}");
    }
    let exact = merge.options().diff == DiffOptions::default() && merge.options().rules.is_empty();
    for (index, chunk) in chunks.iter().enumerate().filter(|_| exact) {
        let [base, left, right] = [(&merge.base, &chunk.base), (&merge.left, &chunk.left), (&merge.right, &chunk.right)].map(|(doc, range)| lines_of(doc, range));
        match chunk.kind {
            ChunkKind::Unchanged => assert!(base == left && left == right, "{chunk:?}"),
            ChunkKind::LeftOnly => assert!(right == base && left != base, "{chunk:?}"),
            ChunkKind::RightOnly => assert!(left == base && right != base, "{chunk:?}"),
            ChunkKind::Both => assert_eq!(left, right, "{chunk:?}"),
            ChunkKind::Conflict => assert_ne!(left, right, "{chunk:?}"),
        }
        if chunk.resolution != Resolution::Edited {
            let shown = lines_of(&merge.result, &chunk.result);
            let expected = merge.resolution_lines(index, chunk.resolution);
            assert_eq!(shown, expected, "{chunk:?}");
        }
    }
}

fn conflict(merge: &Merge) -> usize {
    merge.next(None, Stop::Conflict).expect("a conflict")
}

#[test]
fn changes_made_on_one_side_each_merge_without_conflict() {
    let merge = merge("a\nb\nc\nd\ne\n", "a\nB\nc\nd\ne\n", "a\nb\nc\nD\ne\n");
    assert_eq!(text(&merge.result), "a\nB\nc\nD\ne\n");
    assert_eq!(kinds(&merge), [ChunkKind::Unchanged, ChunkKind::LeftOnly, ChunkKind::Unchanged, ChunkKind::RightOnly, ChunkKind::Unchanged]);
    let counts = merge.counts();
    assert_eq!((counts.conflicts, counts.unresolved, counts.left, counts.right, counts.both), (0, 0, 1, 1, 0));
}

#[test]
fn the_same_change_on_both_sides_is_not_a_conflict() {
    let merge = merge("a\nb\nc\n", "a\nX\nc\nleft end\n", "a\nX\nc\n");
    assert_eq!(text(&merge.result), "a\nX\nc\nleft end\n");
    assert!(kinds(&merge).contains(&ChunkKind::Both));
    assert_eq!(merge.counts().both, 1);
    assert_eq!(merge.unresolved_count(), 0);
}

#[test]
fn different_changes_of_one_line_are_a_conflict_showing_the_base_until_resolved() {
    let mut merge = merge("one\ntwo\nthree\n", "one\nTWO ours\nthree\n", "one\nTWO theirs\nthree\n");
    let index = conflict(&merge);
    let chunk = &merge.chunks()[index];
    assert_eq!((chunk.base.clone(), chunk.left.clone(), chunk.right.clone()), (1..2, 1..2, 1..2));
    assert_eq!(chunk.resolution, Resolution::Unresolved);
    assert!(!chunk.combinable);
    assert_eq!(text(&merge.result), "one\ntwo\nthree\n");
    assert_eq!(merge.unresolved_count(), 1);
    let cases = [
        (Resolution::Left, "one\nTWO ours\nthree\n"),
        (Resolution::Right, "one\nTWO theirs\nthree\n"),
        (Resolution::LeftThenRight, "one\nTWO ours\nTWO theirs\nthree\n"),
        (Resolution::RightThenLeft, "one\nTWO theirs\nTWO ours\nthree\n"),
        (Resolution::Base, "one\ntwo\nthree\n"),
    ];
    for (resolution, expected) in cases {
        let step = merge.set_resolution(index, resolution).unwrap();
        assert_eq!(text(&merge.result), expected, "{resolution:?}");
        assert_eq!(merge.unresolved_count(), 0);
        check(&merge);
        merge.undo(&step);
        assert_eq!(text(&merge.result), "one\ntwo\nthree\n");
        assert_eq!(merge.chunks()[index].resolution, Resolution::Unresolved);
        check(&merge);
    }
    assert!(merge.set_resolution(index, Resolution::Merged).is_none(), "not combinable");
    assert!(merge.set_resolution(index, Resolution::Unresolved).is_none(), "already unresolved");
    assert!(merge.set_resolution(0, Resolution::Unresolved).is_none(), "not a conflict");
}

#[test]
fn adjacent_line_changes_conflict_as_in_git_but_offer_their_combination() {
    let (base, left, right) = ("1\n2\n3\n4\n5\n", "1\n2-ours\n3\n4\n5\n", "1\n2\n3-theirs\n4\n5\n");
    let mut merge = merge(base, left, right);
    let index = conflict(&merge);
    assert_eq!(merge.counts().conflicts, 1);
    assert!(merge.chunks()[index].combinable);
    assert_eq!(merge.counts().combinable, 1);
    assert_eq!(merge.resolution_lines(index, Resolution::Merged), ["2-ours", "3-theirs"]);
    merge.set_resolution(index, Resolution::Merged).unwrap();
    assert_eq!(text(&merge.result), "1\n2-ours\n3-theirs\n4\n5\n");
    assert_eq!(merge.counts().combined, 1);

    let options = MergeOptions { combine_adjacent: true, ..MergeOptions::default() };
    let auto = merge_with(base, left, right, options);
    assert_eq!(text(&auto.result), "1\n2-ours\n3-theirs\n4\n5\n");
    assert_eq!((auto.counts().conflicts, auto.unresolved_count(), auto.counts().combined), (1, 0, 1));
}

#[test]
fn an_insertion_next_to_a_change_combines_on_its_side_of_it() {
    // Left inserts before line 2, right changes line 2: touching, not overlapping.
    let merge = merge("1\n2\n3\n", "1\nnew\n2\n3\n", "1\n2-theirs\n3\n");
    let index = conflict(&merge);
    assert!(merge.chunks()[index].combinable);
    assert_eq!(merge.resolution_lines(index, Resolution::Merged), ["new", "2-theirs"]);
    // Left inserts after line 2 instead.
    let merge = self::merge("1\n2\n3\n", "1\n2\nnew\n3\n", "1\n2-theirs\n3\n");
    let index = conflict(&merge);
    assert_eq!(merge.resolution_lines(index, Resolution::Merged), ["2-theirs", "new"]);
}

#[test]
fn overlapping_changes_have_no_combination() {
    // Both rewrite line 2, left also line 3: overlap on line 2.
    let merge = merge("1\n2\n3\n4\n", "1\nA\nB\n4\n", "1\nC\n3\n4\n");
    let index = conflict(&merge);
    assert!(!merge.chunks()[index].combinable);
    assert!(merge.resolution_lines(index, Resolution::Merged).is_empty());
}

#[test]
fn insertions_at_the_same_place_conflict_with_an_empty_base() {
    let mut merge = merge("a\nb\n", "a\nx\nb\n", "a\ny\nb\n");
    let index = conflict(&merge);
    let chunk = &merge.chunks()[index];
    assert!(chunk.base.is_empty() && !chunk.combinable);
    assert!(chunk.result.is_empty(), "nothing to show while unresolved");
    assert_eq!(text(&merge.result), "a\nb\n");
    merge.set_resolution(index, Resolution::LeftThenRight).unwrap();
    assert_eq!(text(&merge.result), "a\nx\ny\nb\n");
    check(&merge);
    // The same insertion on both sides is merged once.
    assert_eq!(text(&self::merge("a\nb\n", "a\nx\nb\n", "a\nx\nb\n").result), "a\nx\nb\n");
}

#[test]
fn a_deletion_against_a_modification_is_a_conflict() {
    let mut merge = merge("a\nb\nc\n", "a\nc\n", "a\nB\nc\n");
    let index = conflict(&merge);
    assert_eq!(merge.chunks()[index].left, 1..1);
    merge.set_resolution(index, Resolution::Left).unwrap();
    assert_eq!(text(&merge.result), "a\nc\n");
    check(&merge);
    // Deleted on one side, untouched on the other: deleted.
    assert_eq!(text(&self::merge("a\nb\nc\n", "a\nc\n", "a\nb\nc\n").result), "a\nc\n");
}

#[test]
fn changes_at_the_start_and_end_of_the_file_merge() {
    let merge = merge("a\nb\nc\n", "first\na\nb\nc\n", "a\nb\nc\nlast\n");
    assert_eq!(text(&merge.result), "first\na\nb\nc\nlast\n");
    let merge = self::merge("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n");
    assert_eq!(text(&merge.result), "A\nb\nC\n");
    let merge = self::merge("a\nb\nc\n", "A\nb\nc\n", "X\nb\nc\n");
    assert_eq!(merge.chunks()[0].kind, ChunkKind::Conflict);
    let merge = self::merge("a\nb\nc\n", "a\nb\nC\n", "a\nb\nZ\n");
    assert_eq!(merge.chunks().last().unwrap().kind, ChunkKind::Conflict);
}

#[test]
fn files_added_on_both_sides_conflict_only_where_they_differ() {
    // What git passes for an add/add conflict: an empty base file.
    let merge = merge("", "ours added\nsame\n", "theirs added\nsame\n");
    assert_eq!(kinds(&merge), [ChunkKind::Conflict, ChunkKind::Both]);
    assert_eq!(text(&merge.result), "same\n");
    assert_eq!(merge.counts().conflicts, 1);
    assert_eq!(text(&self::merge("", "same\n", "same\n").result), "same\n");
    // All three empty: one empty chunk still owns what will be typed.
    let empty = self::merge("", "", "");
    assert_eq!(empty.chunks().len(), 1);
    assert!(empty.result.is_empty());
}

#[test]
fn a_missing_file_is_an_empty_text_flagged_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    fs::write(path("base"), "a\nb\n").unwrap();
    fs::write(path("remote"), "a\nB\n").unwrap();
    fs::write(path("same"), "a\nb\n").unwrap();
    // Deleted locally, modified remotely: everything conflicts.
    let merge = Merge::open(&path("base"), &path("gone"), &path("remote"), &path("out"), MergeOptions::default()).unwrap();
    check(&merge);
    assert!(merge.is_missing(Version::Left) && !merge.is_missing(Version::Right) && !merge.is_missing(Version::Base));
    assert_eq!(merge.counts().conflicts, 1);
    assert_eq!(merge.chunks()[conflict(&merge)].left, 0..0);
    // Deleted locally, untouched remotely: the result is empty.
    let merge = Merge::open(&path("base"), &path("gone"), &path("same"), &path("out"), MergeOptions::default()).unwrap();
    assert!(merge.result.is_empty() && merge.unresolved_count() == 0);
    // The null device is missing too, as a base for files added on both sides.
    let merge = Merge::open(Path::new("/dev/null"), &path("remote"), &path("same"), &path("out"), MergeOptions::default()).unwrap();
    assert!(merge.is_missing(Version::Base));
    assert_eq!(merge.counts().conflicts, 1);
}

#[test]
fn conflicts_are_trimmed_of_the_lines_both_sides_share() {
    let merge = merge("a\nX\nb\n", "a\nP\nQ\nR\nb\n", "a\nP\nS\nR\nb\n");
    assert_eq!(kinds(&merge), [ChunkKind::Unchanged, ChunkKind::Both, ChunkKind::Conflict, ChunkKind::Both, ChunkKind::Unchanged]);
    let index = conflict(&merge);
    let chunk = &merge.chunks()[index];
    assert_eq!(lines_of(&merge.left, &chunk.left), ["Q"]);
    assert_eq!(lines_of(&merge.right, &chunk.right), ["S"]);
    assert_eq!(lines_of(&merge.base, &chunk.base), ["X"], "the whole base stays with the conflict");
    assert_eq!(text(&merge.result), "a\nP\nX\nR\nb\n");
}

#[test]
fn a_whitespace_change_on_one_side_is_no_conflict_when_whitespace_is_ignored() {
    let (base, left, right) = ("start\n  foo(x);\nend\n", "start\nfoo(x);\nend\n", "start\n  foo(y);\nend\n");
    assert_eq!(merge(base, left, right).counts().conflicts, 1);
    let merge = merge_with(base, left, right, whitespace(Whitespace::IgnoreAll));
    assert_eq!(merge.counts().conflicts, 0);
    assert_eq!(text(&merge.result), "start\n  foo(y);\nend\n", "the side that really changed gives its text");
}

#[test]
fn lines_equal_under_the_options_keep_the_text_of_the_side_that_changed_them() {
    let options = whitespace(Whitespace::IgnoreTrailing);
    // Only the right side added trailing spaces: its text is kept.
    assert_eq!(text(&merge_with("a\nb\n", "a\nb\n", "a  \nb\n", options.clone()).result), "a  \nb\n");
    // Only the left side did.
    assert_eq!(text(&merge_with("a\nb\n", "a \nb\n", "a\nb\n", options.clone()).result), "a \nb\n");
    // Both did, differently: the left text.
    assert_eq!(text(&merge_with("a\nb\n", "a \nb\n", "a   \nb\n", options).result), "a \nb\n");
    let case = MergeOptions { diff: DiffOptions { ignore_case: true, ..DiffOptions::default() }, ..MergeOptions::default() };
    let merge = merge_with("Hello\nx\n", "hello\nx\n", "HELLO\nX\n", case);
    assert_eq!(merge.counts().conflicts, 0);
    assert_eq!(text(&merge.result), "hello\nX\n");
}

#[test]
fn text_ignore_rules_hide_noise_from_the_merge() {
    let date = IGNORE_PRESETS.iter().find(|preset| preset.name == "Date ISO").unwrap().rule();
    let options = MergeOptions { rules: IgnoreRules::new(&[date]).unwrap(), ..MergeOptions::default() };
    let (base, left, right) = ("built 2024-01-01\nx\n", "built 2024-02-02\nx\n", "built 2024-03-03\nx\nnew\n");
    assert_eq!(merge(base, left, right).counts().conflicts, 1);
    let merge = merge_with(base, left, right, options);
    assert_eq!(merge.counts().conflicts, 0);
    assert_eq!(text(&merge.result), "built 2024-02-02\nx\nnew\n");
    // Whole-line rules play no part: a line on one side only is still merged.
    let comments = MergeOptions { rules: IgnoreRules::new(&[IgnoreRule::whole_line("^#")]).unwrap(), ..MergeOptions::default() };
    assert_eq!(text(&merge_with("a\nb\n", "a\n# note\nb\n", "a\nb\n", comments).result), "a\n# note\nb\n");
}

#[test]
fn resolving_returns_an_edit_that_undoes_and_redoes_text_and_state() {
    let mut merge = merge("a\nb\nc\nd\n", "a\nB1\nB2\nc\nd\n", "a\nb-r\nc\nD\n");
    let before = merge.clone();
    let index = conflict(&merge);
    let step = merge.set_resolution(index, Resolution::Left).unwrap();
    assert_eq!(step.edit.at, merge.chunks()[index].result.start as usize);
    assert_eq!(step.edit.removed.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["b"]);
    assert_eq!(step.edit.inserted.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["B1", "B2"]);
    let after = merge.clone();
    assert_eq!(text(&merge.result), "a\nB1\nB2\nc\nD\n");
    merge.undo(&step);
    assert_eq!(merge.chunks(), before.chunks());
    assert_eq!(merge.result.lines, before.result.lines);
    merge.redo(&step);
    assert_eq!(merge.chunks(), after.chunks());
    assert_eq!(merge.result.lines, after.result.lines);
    // A non-conflict can be changed too: revert the right side's change.
    let last = merge.previous(None, Stop::Change).unwrap();
    merge.set_resolution(last, Resolution::Base).unwrap();
    assert_eq!(text(&merge.result), "a\nB1\nB2\nc\nd\n");
    check(&merge);
}

#[test]
fn typing_in_a_chunk_makes_it_edited_and_moves_the_chunks_after_it() {
    let mut merge = merge("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nc\nD\n");
    let original = merge.clone();
    // A new line in the middle of the left change.
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(1, 1), TextPos::new(1, 1), "\nextra");
    let step = merge.track_edit(&edit);
    check(&merge);
    assert_eq!(text(&merge.result), "a\nB\nextra\nc\nD\n");
    let chunk = &merge.chunks()[1];
    assert_eq!((chunk.result.clone(), chunk.resolution), (1..3, Resolution::Edited));
    assert_eq!(merge.chunks()[3].result, 4..5);
    assert_eq!(merge.chunks()[3].resolution, Resolution::Merged);
    assert_eq!(merge.counts().left, 0);
    assert_eq!(merge.counts().edited, 1);
    merge.undo(&step);
    assert_eq!(merge.chunks(), original.chunks());
    assert_eq!(merge.result.lines, original.result.lines);
    merge.redo(&step);
    assert_eq!(text(&merge.result), "a\nB\nextra\nc\nD\n");
}

#[test]
fn an_edit_across_chunks_gives_its_lines_to_the_first_one() {
    let mut merge = merge("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nc\nD\n");
    let original = merge.clone();
    // Join "B" (chunk 1) with "c" (chunk 2) and "D" (chunk 3).
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(1, 1), TextPos::new(3, 0), " ");
    let step = merge.track_edit(&edit);
    check(&merge);
    assert_eq!(text(&merge.result), "a\nB D\n");
    let ranges: Vec<_> = merge.chunks().iter().map(|chunk| (chunk.result.clone(), chunk.resolution)).collect();
    assert_eq!(ranges, [(0..1, Resolution::Merged), (1..2, Resolution::Edited), (2..2, Resolution::Edited), (2..2, Resolution::Edited)]);
    merge.undo(&step);
    assert_eq!(merge.chunks(), original.chunks());
    assert_eq!(merge.result.lines, original.result.lines);
}

#[test]
fn an_edit_that_changes_no_text_changes_no_state() {
    let mut merge = merge("a\nb\n", "a\nB\n", "a\nb\n");
    let original = merge.clone();
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(0, 0), TextPos::new(1, 1), "a\nB");
    merge.track_edit(&edit);
    assert_eq!(merge.chunks(), original.chunks());
}

#[test]
fn typing_in_an_empty_result_has_an_owner() {
    let mut merge = merge("", "", "");
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(0, 0), TextPos::new(0, 0), "hello\nworld");
    let step = merge.track_edit(&edit);
    check(&merge);
    assert_eq!(merge.chunks()[0].resolution, Resolution::Edited);
    assert_eq!(text(&merge.result), "hello\nworld");
    merge.undo(&step);
    assert!(merge.result.is_empty() && merge.chunks()[0].result.is_empty());
    // A conflict with nothing to show takes text inserted where it stands.
    let mut merge = self::merge("a\nb\n", "a\nx\nb\n", "a\ny\nb\n");
    let index = conflict(&merge);
    let at = merge.chunks()[index].result.start as usize;
    let edit = Edit { at, removed: Vec::new(), inserted: vec![Line { text: "mine".into(), eol: Eol::Lf }] };
    merge.result.apply(&edit);
    merge.track_edit(&edit);
    check(&merge);
    assert_eq!(merge.chunks()[index].resolution, Resolution::Edited);
    assert_eq!(merge.chunks()[index].result.len(), 1);
}

#[test]
fn text_edited_with_markers_left_in_stays_unresolved() {
    let mut merge = merge("one\ntwo\n", "one\nours\n", "one\ntheirs\n");
    let index = conflict(&merge);
    let markers = merge.marker_lines(index);
    assert_eq!(markers, ["<<<<<<< ours", "ours", "=======", "theirs", ">>>>>>> theirs"]);
    let step = merge.set_lines(index, markers);
    assert_eq!(merge.chunks()[index].resolution, Resolution::Edited);
    assert!(merge.chunks()[index].has_markers);
    assert_eq!(merge.unresolved_count(), 1);
    assert_eq!(merge.next(None, Stop::Unresolved), Some(index));
    // Removing the markers by typing resolves it.
    let start = merge.chunks()[index].result.start;
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(start, 0), TextPos::new(start + 2, 7), "ours");
    merge.track_edit(&edit);
    let (edit, _) = textedit::replace(&mut merge.result, TextPos::new(start + 1, 0), TextPos::new(start + 2, 14), "");
    merge.track_edit(&edit);
    check(&merge);
    assert_eq!(text(&merge.result), "one\nours\n\n");
    assert_eq!(merge.unresolved_count(), 0);
    let _ = step;
}

#[test]
fn navigation_stops_on_changes_conflicts_and_unresolved_ones() {
    let mut merge = merge("1\n2\n3\n4\n5\n6\n7\n", "1\nA\n3\n4\nL\n6\n7\n", "1\nB\n3\nR\n5\n6\nZ\n");
    let changes: Vec<usize> = std::iter::successors(merge.next(None, Stop::Change), |&i| merge.next(Some(i), Stop::Change)).collect();
    assert_eq!(changes.len(), 3, "{:?}", kinds(&merge));
    let conflicts: Vec<usize> = std::iter::successors(merge.next(None, Stop::Conflict), |&i| merge.next(Some(i), Stop::Conflict)).collect();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(merge.previous(None, Stop::Conflict), Some(conflicts[1]));
    assert_eq!(merge.previous(Some(conflicts[1]), Stop::Conflict), Some(conflicts[0]));
    assert_eq!(merge.previous(Some(conflicts[0]), Stop::Conflict), None);
    merge.set_resolution(conflicts[0], Resolution::Left).unwrap();
    assert_eq!(merge.next(None, Stop::Unresolved), Some(conflicts[1]));
    assert_eq!(merge.next(None, Stop::Conflict), Some(conflicts[0]));
    // Lines map to chunks in every text.
    assert_eq!(merge.chunk_at(Version::Left, 1), Some(conflicts[0]));
    assert_eq!(merge.chunk_at(Version::Result, 0), Some(0));
    assert_eq!(merge.chunk_at(Version::Base, 99), None);
}

#[test]
fn a_conflict_details_what_each_side_changed_word_by_word() {
    let merge = merge("let x = compute(a);\n", "let x = compute_fast(a);\n", "let x = compute(a, b);\n");
    let index = conflict(&merge);
    let left = merge.compare(index, Version::Base, Version::Left);
    let right = merge.compare(index, Version::Base, Version::Right);
    let spans = |result: &DiffResult, text: &str| -> Vec<String> {
        let pair = result.rows[0].pair.unwrap() as usize;
        result.spans[pair].right.iter().map(|span| text[span.range.start as usize..span.range.end as usize].to_owned()).collect()
    };
    assert_eq!(spans(&left, "let x = compute_fast(a);"), ["compute_fast"]);
    assert_eq!(spans(&right, "let x = compute(a, b);"), [", b"]);
    // Line numbers are those of the whole texts.
    let merge = self::merge("h\nx\n", "h\ny\n", "h\nz\n");
    let index = conflict(&merge);
    assert_eq!(merge.compare(index, Version::Left, Version::Right).rows[0].left, Some(1));
}

/// What git 2.56 wrote for these conflicts (see the module report): merge style after `git
/// merge`, and zdiff3 from `git checkout --conflict=zdiff3`.
const GIT_MERGE: &str = "one\n<<<<<<< HEAD\nTWO-ours\n=======\nTWO-theirs\n>>>>>>> other\nthree\nfour\nFIVE\n";
const GIT_ZDIFF3: &str = "one\n<<<<<<< ours\nTWO-ours\n||||||| base\ntwo\n=======\nTWO-theirs\n>>>>>>> theirs\nthree\nfour\nFIVE\n";
const GIT_ADD_DIFF3: &str = "<<<<<<< ours\nours added\nsame\n||||||| base\n=======\ntheirs added\nsame\n>>>>>>> theirs\n";
const GIT_ADD_ZDIFF3: &str = "<<<<<<< ours\nours added\n||||||| base\n=======\ntheirs added\n>>>>>>> theirs\nsame\n";

#[test]
fn unresolved_conflicts_are_written_as_git_writes_them() {
    let mut merge = merge("one\ntwo\nthree\nfour\nfive\n", "one\nTWO-ours\nthree\nfour\nfive\n", "one\nTWO-theirs\nthree\nfour\nFIVE\n");
    merge.labels = MarkerLabels { left: "HEAD".into(), base: "8769f7a".into(), right: "other".into() };
    assert_eq!(marked(&merge), GIT_MERGE);
    merge.labels = MarkerLabels::default();
    merge.marker_style = MarkerStyle::Diff3;
    assert_eq!(marked(&merge), GIT_ZDIFF3);
    let mut added = self::merge("", "ours added\nsame\n", "theirs added\nsame\n");
    added.marker_style = MarkerStyle::Diff3;
    assert_eq!(marked(&added), GIT_ADD_ZDIFF3);
}

#[test]
fn a_conflicted_file_rebuilds_its_three_versions() {
    let merge = Merge::from_conflicted(doc(GIT_ZDIFF3), MergeOptions::default()).unwrap();
    check(&merge);
    assert!(merge.base_known());
    assert_eq!(text(&merge.left), "one\nTWO-ours\nthree\nfour\nFIVE\n");
    assert_eq!(text(&merge.right), "one\nTWO-theirs\nthree\nfour\nFIVE\n");
    assert_eq!(text(&merge.base), "one\ntwo\nthree\nfour\nFIVE\n");
    assert_eq!(text(&merge.result), "one\ntwo\nthree\nfour\nFIVE\n");
    assert_eq!(merge.unresolved_count(), 1);
    assert_eq!(merge.marker_style, MarkerStyle::Diff3);
    assert_eq!(marked(&merge), GIT_ZDIFF3, "written back as it came");

    let merge = Merge::from_conflicted(doc(GIT_MERGE), MergeOptions::default()).unwrap();
    check(&merge);
    assert!(!merge.base_known());
    assert_eq!(text(&merge.base), "one\nthree\nfour\nFIVE\n");
    assert_eq!(merge.labels, MarkerLabels { left: "HEAD".into(), base: String::new(), right: "other".into() });
    assert_eq!(text(&merge.result), "one\nthree\nfour\nFIVE\n");
    assert_eq!(marked(&merge), GIT_MERGE);

    // Git's plain diff3 does not trim: the shared line leaves the conflict here.
    let merge = Merge::from_conflicted(doc(GIT_ADD_DIFF3), MergeOptions::default()).unwrap();
    check(&merge);
    assert_eq!(kinds(&merge), [ChunkKind::Conflict, ChunkKind::Both]);
    assert_eq!(text(&merge.left), "ours added\nsame\n");
    assert_eq!(marked(&merge), GIT_ADD_ZDIFF3);
}

#[test]
fn text_that_only_looks_like_markers_is_text() {
    let text = "Title\n=======\n<<<<<<<< eight\n<<<<<<<glued\n>>>>>>> stray\n||||||| stray\nend\n";
    assert!(!has_conflict_markers(&doc(text).lines));
    let merge = Merge::from_conflicted(doc(text), MergeOptions::default()).unwrap();
    assert_eq!(merge.counts().conflicts, 0);
    assert_eq!(self::text(&merge.result), text);
    // Inside the right section, only the closing marker counts.
    let file = "<<<<<<<\nleft\n=======\nHeading\n=======\n||||||| x\n>>>>>>>\n";
    let merge = Merge::from_conflicted(doc(file), MergeOptions::default()).unwrap();
    assert_eq!(self::text(&merge.right), "Heading\n=======\n||||||| x\n");
    assert_eq!(marked(&merge), file);
    assert!(has_conflict_markers(&doc(file).lines));
}

#[test]
fn malformed_markers_are_an_error_naming_the_line() {
    for (file, line) in [
        ("a\n<<<<<<< x\nleft\n", 2),
        ("<<<<<<< x\nleft\n<<<<<<< y\n=======\n>>>>>>> z\n", 3),
        ("<<<<<<< x\nleft\n>>>>>>> z\n", 3),
    ] {
        match Merge::from_conflicted(doc(file), MergeOptions::default()) {
            Err(error @ MergeError::Markers { line: found, .. }) => {
                assert_eq!(found, line, "{file:?}");
                assert!(error.to_string().contains("marqueurs de conflit mal formés"));
            }
            other => panic!("{file:?}: {other:?}"),
        }
        assert!(!has_conflict_markers(&doc(file).lines));
    }
}

#[test]
fn line_endings_and_encoding_follow_the_side_that_changed_them() {
    // Left converted the file to CRLF, right changed a line: CRLF everywhere.
    let merge = merge("a\nb\nc\n", "a\r\nb\r\nc\r\n", "a\nB\nc\n");
    assert_eq!(text(&merge.result), "a\r\nB\r\nc\r\n");
    // Right converted it, left changed a line: CRLF too, left's lines take it.
    let merge = self::merge("a\nb\nc\n", "a\nB\nc\n", "a\r\nb\r\nc\r\n");
    assert_eq!(text(&merge.result), "a\r\nB\r\nc\r\n");
    // Mixed endings of the left side are kept where its lines are.
    let merge = self::merge("a\nb\nc\nd\n", "a\r\nb\nc\r\nd\r\n", "a\nb\nc\nD\n");
    assert_eq!(text(&merge.result), "a\r\nb\nc\r\nD\r\n");
    // Markers use the result's ending, as git does.
    let merge = self::merge("a\r\nb\r\n", "a\r\nx\r\n", "a\r\ny\r\n");
    assert_eq!(marked(&merge), "a\r\n<<<<<<< ours\r\nx\r\n=======\r\ny\r\n>>>>>>> theirs\r\n");
    // The final newline: removed on one side only, it is removed.
    assert_eq!(text(&self::merge("a\nb\n", "a\nb", "A\nb\n").result), "A\nb");
    assert_eq!(text(&self::merge("a\nb", "a\nb\n", "a\nB").result), "a\nB\n");

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    fs::write(path("base"), "a\nb\n").unwrap();
    fs::write(path("local"), "a\nB\n").unwrap();
    fs::write(path("remote"), b"\xEF\xBB\xBFa\nb\n").unwrap();
    let merge = Merge::open(&path("base"), &path("local"), &path("remote"), &path("out"), MergeOptions::default()).unwrap();
    assert_eq!(merge.result.encoding, TextEncoding::Utf8Bom, "the right side added a BOM");
    assert_eq!(merge.result.encode().unwrap(), b"\xEF\xBB\xBFa\nB\n");
    fs::write(path("latin"), b"caf\xE9\nb\n").unwrap();
    fs::write(path("latin-base"), b"caf\xE9\nb\n").unwrap();
    let merge = Merge::open(&path("latin-base"), &path("latin"), &path("latin-base"), &path("out"), MergeOptions::default()).unwrap();
    assert_eq!(merge.result.encoding, TextEncoding::Windows1252);
    assert_eq!(merge.left.lines[0].text, "café");
}

#[test]
fn binary_and_undecodable_inputs_are_refused_in_french() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    fs::write(path("text"), "a\n").unwrap();
    fs::write(path("binary"), b"a\0b").unwrap();
    fs::write(path("broken"), b"\xFF\xFE\x00\xD8").unwrap();
    let error = Merge::open(&path("text"), &path("text"), &path("binary"), &path("out"), MergeOptions::default()).unwrap_err();
    assert!(matches!(error, MergeError::Binary { version: Version::Right, .. }));
    assert!(error.to_string().ends_with("(version distante) : fichier binaire, fusion impossible"), "{error}");
    let error = Merge::open(&path("broken"), &path("text"), &path("text"), &path("out"), MergeOptions::default()).unwrap_err();
    assert!(error.to_string().contains("(ancêtre commun) : texte indécodable"), "{error}");
}

#[test]
fn saving_is_refused_while_conflicts_remain_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    fs::write(path("base"), "one\nmid\ntwo\n").unwrap();
    fs::write(path("local"), "one\nmid\nours\n").unwrap();
    fs::write(path("remote"), "ONE\nmid\ntheirs\n").unwrap();
    fs::write(path("merged"), "what git left\n").unwrap();
    let mut merge = Merge::open(&path("base"), &path("local"), &path("remote"), &path("merged"), MergeOptions::default()).unwrap();
    let error = merge.save(false).unwrap_err();
    assert_eq!(error.to_string(), "1 conflit non résolu : enregistrement refusé");
    assert_eq!(fs::read_to_string(path("merged")).unwrap(), "what git left\n");

    merge.save(true).unwrap();
    assert_eq!(fs::read_to_string(path("merged")).unwrap(), "ONE\nmid\n<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n");
    assert_eq!(text(&merge.result), "ONE\nmid\ntwo\n", "the result keeps its lines");
    // Saving again after a resolution: the disk stamp was updated by the forced save.
    merge.set_resolution(conflict(&merge), Resolution::Right).unwrap();
    merge.save(false).unwrap();
    assert_eq!(fs::read_to_string(path("merged")).unwrap(), "ONE\nmid\ntheirs\n");
    // A file changed behind our back is not overwritten.
    fs::write(path("merged"), "changed elsewhere, longer\n").unwrap();
    assert!(matches!(merge.save(false), Err(MergeError::Document(DocumentError::ChangedOnDisk(_)))));
}

#[test]
fn a_conflicted_working_file_opens_and_saves_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, GIT_MERGE.replace('\n', "\r\n")).unwrap();
    assert!(has_conflict_markers(&Document::load(&file).unwrap().lines));
    let mut merge = Merge::open_conflicted(&file, MergeOptions::default()).unwrap();
    let index = conflict(&merge);
    merge.set_resolution(index, Resolution::LeftThenRight).unwrap();
    merge.save(false).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "one\r\nTWO-ours\r\nTWO-theirs\r\nthree\r\nfour\r\nFIVE\r\n");
}

/// A small deterministic generator: no dependency for the randomized test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const WORDS: [&str; 7] = ["a", "b", "c", "d", "{", "}", ""];
/// With spaces and case, for the runs where the options hide them.
const LOOSE_WORDS: [&str; 7] = ["a", "a ", " a", "A", "{", "}", ""];

fn random_text(rng: &mut Rng, words: &[&str]) -> Vec<String> {
    (0..rng.below(12)).map(|_| words[rng.below(words.len())].to_owned()).collect()
}

/// A random edit of `base`: lines removed, replaced and inserted.
fn mutate(rng: &mut Rng, base: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in base {
        match rng.below(10) {
            0 => {}
            1 => out.push(format!("{line}x{}", rng.below(3))),
            2 => out.push(format!("{} ", line.to_uppercase())),
            3 => {
                out.push(line.clone());
                out.push(["new", "a", "}"][rng.below(3)].to_owned());
            }
            _ => out.push(line.clone()),
        }
    }
    if rng.below(4) == 0 {
        out.insert(rng.below(out.len() + 1), "tail".to_owned());
    }
    out
}

fn join(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

#[test]
fn random_merges_keep_every_invariant() {
    random_merges(MergeOptions::default(), &WORDS, 0x9E37_79B9_7F4A_7C15);
}

#[test]
fn random_merges_keep_every_invariant_when_options_hide_differences() {
    let diff = DiffOptions { whitespace: Whitespace::IgnoreAll, ignore_case: true };
    random_merges(MergeOptions { diff, ..MergeOptions::default() }, &LOOSE_WORDS, 0x2545_F491_4F6C_DD1D);
}

/// Exact invariants of random merges; with options the texts kept must still be real ones.
fn random_merges(options: MergeOptions, words: &[&str], seed: u64) {
    let mut rng = Rng(seed);
    // How often the interesting cases came up, so that the generator is known to reach them.
    let (mut conflicts, mut combinable, mut spanning) = (0, 0, 0);
    for round in 0..1500 {
        let base = random_text(&mut rng, words);
        let left = if rng.below(6) == 0 { base.clone() } else if rng.below(8) == 0 { random_text(&mut rng, words) } else { mutate(&mut rng, &base) };
        let right = match rng.below(8) {
            0 => base.clone(),
            1 => left.clone(),
            2 => random_text(&mut rng, words),
            _ => mutate(&mut rng, &base),
        };
        let (b, l, r) = (join(&base), join(&left), join(&right));
        let context = format!("round {round}\nbase {b:?}\nleft {l:?}\nright {r:?}");
        let mut merge = merge_with(&b, &l, &r, options.clone());
        let counts = merge.counts();
        if left == base {
            assert_eq!(text(&merge.result), r, "{context}");
            assert_eq!(counts.conflicts, 0, "{context}");
        }
        if right == base {
            assert_eq!(text(&merge.result), l, "{context}");
            assert_eq!(counts.conflicts, 0, "{context}");
        }
        if left == right {
            assert_eq!(text(&merge.result), l, "{context}");
            assert_eq!(counts.conflicts, 0, "{context}");
        }
        assert_eq!(counts.unresolved, counts.conflicts, "{context}");
        conflicts += counts.conflicts;
        combinable += counts.combinable;

        // Markers round trip, in both styles.
        for style in [MarkerStyle::Merge, MarkerStyle::Diff3] {
            merge.marker_style = style;
            let file = marked(&merge);
            let parsed = Merge::from_conflicted(doc(&file), options.clone()).unwrap();
            check(&parsed);
            assert_eq!(parsed.unresolved_count(), counts.unresolved, "{context}\n{file}");
            assert_eq!(marked(&parsed), file, "{context}");
            let mut sided = merge.clone();
            while let Some(index) = sided.next(None, Stop::Unresolved) {
                sided.set_resolution(index, Resolution::Left).unwrap();
            }
            assert_eq!(text(&parsed.left), text(&sided.result), "{context}\n{file}");
        }

        // Taking one text everywhere gives that text; undoing everything gives the merge back.
        let original = merge.clone();
        for (resolution, expected) in [(Resolution::Left, &l), (Resolution::Right, &r), (Resolution::Base, &b)] {
            let mut steps = Vec::new();
            for index in 0..merge.chunks().len() {
                steps.extend(merge.set_resolution(index, resolution));
            }
            check(&merge);
            assert_eq!(&text(&merge.result), expected, "{context}\n{resolution:?}");
            for step in steps.iter().rev() {
                merge.undo(step);
            }
            assert_eq!(merge.chunks(), original.chunks(), "{context}");
            assert_eq!(merge.result.lines, original.result.lines, "{context}");
        }

        // Random typing keeps the mapping, and undoes back to the merge.
        let mut steps = Vec::new();
        for _ in 0..4 {
            let len = merge.result.len() as u32;
            let pos = |rng: &mut Rng, from: u32| {
                if len == 0 {
                    return TextPos::new(0, 0);
                }
                let line = from + rng.below((len - from) as usize) as u32;
                let width = merge.result.lines[line as usize].text.len();
                TextPos::new(line, if rng.below(2) == 0 { 0 } else { width })
            };
            let start = pos(&mut rng, 0);
            let end = if rng.below(2) == 0 { start } else { pos(&mut rng, start.line) };
            let end = if end.line == start.line && end.byte < start.byte { start } else { end };
            let typed = ["", "z", "\n", "q\nr"][rng.below(4)];
            let (edit, _) = textedit::replace(&mut merge.result, start, end, typed);
            let step = merge.track_edit(&edit);
            spanning += (step.before.len() > 1) as usize;
            steps.push(step);
            check(&merge);
        }
        for step in steps.iter().rev() {
            merge.undo(step);
            check(&merge);
        }
        assert_eq!(merge.chunks(), original.chunks(), "{context}");
        assert_eq!(merge.result.lines, original.result.lines, "{context}");
    }
    eprintln!("random merges: {conflicts} conflicts, {combinable} combinable, {spanning} edits across chunks");
    assert!(conflicts > 300 && combinable > 30 && spanning > 300, "{conflicts} {combinable} {spanning}");
}
