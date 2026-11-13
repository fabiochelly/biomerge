//! Diff3 chunking on comparison keys, zealous trimming of conflicts, and the proposal that
//! combines two sides whose changes do not overlap.

use std::ops::Range;

use imara_diff::Hunk;

use super::{Chunk, ChunkKind, Piece, Resolution, Version};
use crate::diff::line_hunks;

/// The comparison keys of the three texts, whole: chunk ranges index them directly.
pub(super) struct Keys<'a> {
    pub base: &'a [String],
    pub left: &'a [String],
    pub right: &'a [String],
}

fn keys<'a>(keys: &'a [String], range: &Range<u32>) -> &'a [String] {
    &keys[range.start as usize..range.end as usize]
}

pub(super) fn chunk_of(kind: ChunkKind, base: Range<u32>, left: Range<u32>, right: Range<u32>) -> Chunk {
    Chunk {
        kind,
        base,
        left,
        right,
        result: 0..0,
        resolution: Resolution::Merged,
        combinable: false,
        has_markers: false,
        combined: Vec::new(),
    }
}

/// The chunks of a whole merge, in order. Every line of each text is in exactly one chunk, and
/// there is always at least one chunk, so that text typed into an empty result has an owner.
///
/// Classic diff3: the hunks of base→left and base→right are walked together in base order, and
/// hunks that overlap *or touch* (one starts where the other ends, an insertion at the edge of
/// a change, two insertions at the same place) make one region, as in git: changes to
/// adjacent lines are a conflict. A region changed by one side only takes that side, one
/// changed identically on both sides (under the options) takes the left text, and the rest is
/// handed to [`conflict_chunks`].
pub(super) fn chunks(keys: &Keys) -> Vec<Chunk> {
    let (left_hunks, right_hunks) =
        rayon::join(|| line_hunks(keys.base, keys.left), || line_hunks(keys.base, keys.right));
    let (a, b) = (&left_hunks, &right_hunks);
    let at = |line: u32, delta: i64| (line as i64 + delta) as u32;
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    // Base lines before `pos` are in a chunk; outside hunks a side's line is the base's + delta.
    let (mut pos, mut left_delta, mut right_delta) = (0u32, 0i64, 0i64);
    while i < a.len() || j < b.len() {
        let start = a.get(i).map_or(u32::MAX, |h| h.before.start).min(b.get(j).map_or(u32::MAX, |h| h.before.start));
        if start > pos {
            out.push(chunk_of(
                ChunkKind::Unchanged,
                pos..start,
                at(pos, left_delta)..at(start, left_delta),
                at(pos, right_delta)..at(start, right_delta),
            ));
        }
        let (i0, j0) = (i, j);
        let mut end = start;
        loop {
            if let Some(hunk) = a.get(i).filter(|h| h.before.start <= end) {
                end = end.max(hunk.before.end);
                i += 1;
            } else if let Some(hunk) = b.get(j).filter(|h| h.before.start <= end) {
                end = end.max(hunk.before.end);
                j += 1;
            } else {
                break;
            }
        }
        let side = |hunks: &[Hunk], from: usize, to: usize, delta: i64| -> Range<u32> {
            if from == to {
                return at(start, delta)..at(end, delta);
            }
            let (first, last) = (&hunks[from], &hunks[to - 1]);
            first.after.start - (first.before.start - start)..last.after.end + (end - last.before.end)
        };
        let left = side(a, i0, i, left_delta);
        let right = side(b, j0, j, right_delta);
        left_delta = left.end as i64 - end as i64;
        right_delta = right.end as i64 - end as i64;
        match (i > i0, j > j0) {
            (true, false) => out.push(chunk_of(ChunkKind::LeftOnly, start..end, left, right)),
            (false, true) => out.push(chunk_of(ChunkKind::RightOnly, start..end, left, right)),
            _ => conflict_chunks(keys, start..end, left, right, &mut out),
        }
        pos = end;
    }
    let base_len = keys.base.len() as u32;
    if pos < base_len || out.is_empty() {
        out.push(chunk_of(
            ChunkKind::Unchanged,
            pos..base_len,
            at(pos, left_delta)..keys.left.len() as u32,
            at(pos, right_delta)..keys.right.len() as u32,
        ));
    }
    out
}

/// Chunks of a region both sides changed. Equal sides make one `Both` chunk. Otherwise the
/// lines the two sides share at the start and at the end leave the conflict (git's "zealous"
/// trimming, what zdiff3 shows): as `Unchanged` where the base has them too, else as `Both`
/// with an empty base range; the whole remaining base stays with the conflict. The remaining
/// core is a conflict, flagged `combinable` when its two sides changed different base lines.
pub(super) fn conflict_chunks(keys: &Keys, base: Range<u32>, left: Range<u32>, right: Range<u32>, out: &mut Vec<Chunk>) {
    let (base_keys, left_keys, right_keys) = (self::keys(keys.base, &base), self::keys(keys.left, &left), self::keys(keys.right, &right));
    if left_keys == right_keys {
        out.push(chunk_of(ChunkKind::Both, base, left, right));
        return;
    }
    let prefix = common_prefix(left_keys, right_keys);
    let suffix = common_suffix(&left_keys[prefix..], &right_keys[prefix..]) as u32;
    let base_prefix = common_prefix(&left_keys[..prefix], base_keys);
    let base_suffix = common_suffix(&left_keys[left_keys.len() - suffix as usize..], &base_keys[base_prefix..]) as u32;
    let (prefix, base_prefix) = (prefix as u32, base_prefix as u32);
    let (b, l, r) = (base.start, left.start, right.start);
    if base_prefix > 0 {
        out.push(chunk_of(ChunkKind::Unchanged, b..b + base_prefix, l..l + base_prefix, r..r + base_prefix));
    }
    if prefix > base_prefix {
        let b = b + base_prefix;
        out.push(chunk_of(ChunkKind::Both, b..b, l + base_prefix..l + prefix, r + base_prefix..r + prefix));
    }
    let core = [base.start + base_prefix..base.end - base_suffix, left.start + prefix..left.end - suffix, right.start + prefix..right.end - suffix];
    let combined = combination(keys, &core);
    let [core_base, core_left, core_right] = core;
    out.push(Chunk {
        combinable: combined.is_some(),
        combined: combined.unwrap_or_default(),
        resolution: Resolution::Unresolved,
        ..chunk_of(ChunkKind::Conflict, core_base, core_left, core_right)
    });
    let (b, l, r) = (base.end, left.end, right.end);
    if suffix > base_suffix {
        let b = b - base_suffix;
        out.push(chunk_of(ChunkKind::Both, b..b, l - suffix..l - base_suffix, r - suffix..r - base_suffix));
    }
    if base_suffix > 0 {
        out.push(chunk_of(ChunkKind::Unchanged, b - base_suffix..b, l - base_suffix..l, r - base_suffix..r));
    }
}

/// Both sides' changes applied to the base of a conflict, when no change of one side overlaps
/// a change of the other: ranges sharing a base line, an insertion strictly inside the other
/// side's replaced lines, or two different insertions at the same place (their order would be
/// a guess). Touching is allowed, which is the adjacent-lines case git calls a conflict: an
/// insertion at the edge of a replaced range goes on its side of it. A change made identically
/// by both sides is applied once.
fn combination(keys: &Keys, core: &[Range<u32>; 3]) -> Option<Vec<Piece>> {
    let [base, left, right] = core;
    let (base_keys, left_keys, right_keys) = (self::keys(keys.base, base), self::keys(keys.left, left), self::keys(keys.right, right));
    let (left_hunks, right_hunks) = (line_hunks(base_keys, left_keys), line_hunks(base_keys, right_keys));
    let mut all: Vec<(Version, Hunk)> = left_hunks.into_iter().map(|h| (Version::Left, h)).collect();
    all.extend(right_hunks.into_iter().map(|h| (Version::Right, h)));
    all.sort_by_key(|(_, h)| (h.before.start, h.before.end));
    let replacement = |(version, hunk): &(Version, Hunk)| match version {
        Version::Left => self::keys(left_keys, &hunk.after),
        _ => self::keys(right_keys, &hunk.after),
    };
    let mut kept: Vec<(Version, Hunk)> = Vec::with_capacity(all.len());
    // The furthest base line reached by the changes kept so far.
    let mut reach = 0;
    for entry in all {
        if let Some(previous) = kept.last() {
            if previous.1.before == entry.1.before && replacement(previous) == replacement(&entry) {
                continue;
            }
            let same_insertion = previous.1.before.is_empty() && entry.1.before == previous.1.before;
            if entry.1.before.start < reach || same_insertion {
                return None;
            }
        }
        reach = reach.max(entry.1.before.end);
        kept.push(entry);
    }
    let mut pieces = Vec::new();
    let mut push = |version: Version, range: Range<u32>| {
        if !range.is_empty() {
            pieces.push(Piece { version, range });
        }
    };
    let mut cursor = 0;
    for (version, hunk) in kept {
        push(Version::Base, base.start + cursor..base.start + hunk.before.start);
        let offset = if version == Version::Left { left.start } else { right.start };
        push(version, offset + hunk.after.start..offset + hunk.after.end);
        cursor = hunk.before.end;
    }
    push(Version::Base, base.start + cursor..base.end);
    Some(pieces)
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[String], b: &[String]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}
