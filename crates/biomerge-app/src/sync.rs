//! Synchronized scrolling without filler lines, as in Araxis Merge.
//!
//! Each pane keeps its own lines. A shared position runs along a virtual document in which a
//! block is as tall as its taller side: the taller side scrolls normally while the other one
//! slows down, or waits when it has no line there, and both meet again after the block.
//! The panes agree in the middle of the view; near the ends of the file that point slides to
//! the edge of the view, so that both the first and the last line can be reached on both sides.
//!
//! Any number of panes works the same way: the three of a merge share one position, and a chunk
//! is as tall as its tallest pane.
//!
//! All positions are in visual lines.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment<const N: usize> {
    start: u32,
    length: u32,
    panes: [(u32, u32); N],
}

#[derive(Debug)]
pub struct SyncMap<const N: usize = 2> {
    segments: Vec<Segment<N>>,
    /// Virtual extent of each block.
    pub blocks: Vec<Range<u32>>,
    total: u32,
    pane_totals: [u32; N],
}

impl<const N: usize> Default for SyncMap<N> {
    fn default() -> Self {
        Self { segments: Vec::new(), blocks: Vec::new(), total: 0, pane_totals: [0; N] }
    }
}

impl<const N: usize> SyncMap<N> {
    /// `hunks`: the line ranges of each block on every pane. `offsets`: for each pane, the first
    /// visual line of each line, with one more entry holding the pane's total.
    pub fn new(hunks: impl Iterator<Item = [Range<u32>; N]>, offsets: [&[u32]; N]) -> Self {
        let visual = |pane: usize, line: u32| offsets[pane].get(line as usize).copied().unwrap_or(0);
        let ends: [u32; N] = std::array::from_fn(|pane| offsets[pane].last().copied().unwrap_or(0));
        let mut map = Self {
            pane_totals: ends,
            ..Self::default()
        };
        let mut lines = [0u32; N];
        for ranges in hunks {
            let from = std::array::from_fn(|pane| visual(pane, lines[pane]));
            let starts = std::array::from_fn(|pane| visual(pane, ranges[pane].start));
            map.push(from, starts);
            let start = map.total;
            map.push(starts, std::array::from_fn(|pane| visual(pane, ranges[pane].end)));
            map.blocks.push(start..map.total);
            lines = std::array::from_fn(|pane| ranges[pane].end);
        }
        map.push(std::array::from_fn(|pane| visual(pane, lines[pane])), ends);
        map
    }

    fn push(&mut self, from: [u32; N], to: [u32; N]) {
        // Saturating: a pane whose lines were edited since its offsets were taken must not
        // underflow, only stand still.
        let heights: [u32; N] = std::array::from_fn(|pane| to[pane].saturating_sub(from[pane]));
        let length = heights.iter().copied().max().unwrap_or(0);
        if length > 0 {
            self.segments.push(Segment {
                start: self.total,
                length,
                panes: std::array::from_fn(|pane| (from[pane], heights[pane])),
            });
            self.total += length;
        }
    }

    pub fn total(&self) -> f64 {
        self.total as f64
    }

    pub fn max_scroll(&self, view: f64) -> f64 {
        (self.total() - view).max(0.0)
    }

    /// Where a virtual position falls on each side.
    fn at(&self, position: f64) -> [f64; N] {
        let index = self
            .segments
            .partition_point(|s| ((s.start + s.length) as f64) <= position);
        let Some(segment) = self.segments.get(index) else {
            return self.pane_totals.map(f64::from);
        };
        let progress = ((position - segment.start as f64) / segment.length as f64).max(0.0);
        segment.panes.map(|(start, height)| start as f64 + progress * height as f64)
    }

    /// Distance from the top of the view to the line where the panes agree.
    fn focal_offset(&self, scroll: f64, view: f64) -> f64 {
        let max = self.max_scroll(view);
        if max <= 0.0 {
            return 0.0;
        }
        let ramp = (view / 2.0).min(max / 2.0);
        if scroll < ramp {
            scroll / ramp * view / 2.0
        } else if scroll > max - ramp {
            view - (max - scroll) / ramp * view / 2.0
        } else {
            view / 2.0
        }
    }

    /// The scroll position of each pane for a shared position.
    pub fn pane_scrolls(&self, scroll: f64, view: f64) -> [f64; N] {
        let offset = self.focal_offset(scroll, view);
        let at = self.at(scroll + offset);
        std::array::from_fn(|pane| (at[pane] - offset).clamp(0.0, (self.pane_totals[pane] as f64 - view).max(0.0)))
    }

    /// The virtual position shown where the panes agree.
    pub fn focal(&self, scroll: f64, view: f64) -> f64 {
        scroll + self.focal_offset(scroll, view)
    }

    /// The shared position that brings a virtual position where the panes agree.
    pub fn scroll_showing(&self, position: f64, view: f64) -> f64 {
        (position - view / 2.0).clamp(0.0, self.max_scroll(view))
    }

    /// A position that survives a change of wrapping: its segment and its progress inside it.
    pub fn locate(&self, position: f64) -> (usize, f64) {
        let index = self
            .segments
            .partition_point(|s| ((s.start + s.length) as f64) <= position);
        match self.segments.get(index) {
            Some(segment) => (index, (position - segment.start as f64) / segment.length as f64),
            None => (index, 0.0),
        }
    }

    pub fn position_of(&self, located: (usize, f64)) -> f64 {
        self.segments
            .get(located.0)
            .map_or(self.total(), |s| s.start as f64 + located.1 * s.length as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One visual line per line.
    fn offsets(lines: u32) -> Vec<u32> {
        (0..=lines).collect()
    }

    fn map(left: u32, right: u32, hunks: Vec<[Range<u32>; 2]>) -> SyncMap {
        SyncMap::new(hunks.into_iter(), [&offsets(left), &offsets(right)])
    }

    const VIEW: f64 = 40.0;

    #[test]
    fn identical_sides_scroll_together_one_to_one() {
        let sync = map(200, 200, vec![]);
        for scroll in [0.0, 7.5, 20.0, 100.0, 155.0, 160.0] {
            assert_eq!(sync.pane_scrolls(scroll, VIEW), [scroll, scroll]);
        }
    }

    #[test]
    fn a_block_on_one_side_leaves_no_blank_and_the_other_side_waits() {
        // 100 lines inserted on the right after line 100.
        let sync = map(200, 300, vec![[100..100, 100..200]]);
        assert_eq!(sync.total(), 300.0);
        // While the middle of the view is inside the block, the left pane does not move.
        let waiting = sync.pane_scrolls(100.0, VIEW)[0];
        assert_eq!(sync.pane_scrolls(120.0, VIEW)[0], waiting);
        assert_eq!(sync.pane_scrolls(100.0, VIEW)[1], 100.0);
        assert_eq!(sync.pane_scrolls(120.0, VIEW)[1], 120.0);
        // After it, both move again with the right pane 100 lines ahead.
        let [left, right] = sync.pane_scrolls(220.0, VIEW);
        assert_eq!(right - left, 100.0);
    }

    #[test]
    fn a_modified_block_scrolls_its_shorter_side_slower() {
        let sync = map(120, 150, vec![[50..60, 50..90]]);
        // Middle of the block: focal line at virtual 70.
        let [left, right] = sync.pane_scrolls(70.0 - VIEW / 2.0, VIEW);
        assert_eq!(left + VIEW / 2.0, 55.0);
        assert_eq!(right + VIEW / 2.0, 70.0);
    }

    #[test]
    fn both_ends_of_the_file_are_reachable_on_both_sides() {
        let sync = map(100, 300, vec![[0..0, 0..100], [100..100, 200..300]]);
        assert_eq!(sync.pane_scrolls(0.0, VIEW), [0.0, 0.0]);
        let max = sync.max_scroll(VIEW);
        assert_eq!(sync.pane_scrolls(max, VIEW), [60.0, 260.0]);
    }

    #[test]
    fn a_short_file_stays_at_the_top() {
        let sync = map(10, 12, vec![[5..5, 5..7]]);
        assert_eq!(sync.max_scroll(VIEW), 0.0);
        assert_eq!(sync.pane_scrolls(0.0, VIEW), [0.0, 0.0]);
    }

    #[test]
    fn scrolling_never_jumps() {
        let sync = map(500, 650, vec![[10..30, 10..12], [200..200, 182..400], [450..500, 600..650]]);
        let max = sync.max_scroll(VIEW);
        let mut previous = sync.pane_scrolls(0.0, VIEW);
        let mut scroll = 0.25;
        while scroll <= max {
            let current = sync.pane_scrolls(scroll, VIEW);
            for side in 0..2 {
                assert!((current[side] - previous[side]).abs() < 2.5, "at {scroll}: {previous:?} {current:?}");
            }
            previous = current;
            scroll += 0.25;
        }
    }

    #[test]
    fn a_block_is_brought_where_the_panes_agree() {
        let sync = map(500, 600, vec![[300..300, 300..400]]);
        let start = sync.blocks[0].start as f64;
        let scroll = sync.scroll_showing(start, VIEW);
        let [left, right] = sync.pane_scrolls(scroll, VIEW);
        assert_eq!(left, right);
        assert_eq!(left + VIEW / 2.0, 300.0);
    }

    #[test]
    fn wrapped_lines_count_with_their_height() {
        // Line 1 takes 3 visual lines on the left.
        let left = [0, 1, 4, 5];
        let right = [0, 1, 2, 3];
        let sync = SyncMap::new([[1..2, 1..2]].into_iter(), [&left, &right]);
        assert_eq!(sync.blocks, vec![1..4]);
        assert_eq!(sync.total(), 5.0);
    }

    #[test]
    fn a_position_survives_a_new_layout() {
        let before = map(100, 150, vec![[50..50, 50..100]]);
        let located = before.locate(75.0);
        let wider = SyncMap::new([[50..50, 50..100]].into_iter(), [&offsets(100), &offsets(150)]);
        assert_eq!(wider.position_of(located), 75.0);
    }

    #[test]
    fn three_panes_meet_after_a_chunk_as_tall_as_its_tallest_pane() {
        let offsets = [offsets(100), offsets(130), offsets(110)];
        let sync = SyncMap::new([[50..50, 50..80, 50..60]].into_iter(), [&offsets[0], &offsets[1], &offsets[2]]);
        assert_eq!(sync.blocks, vec![50..80]);
        assert_eq!(sync.total(), 130.0);
        let start = sync.scroll_showing(50.0, VIEW);
        let scrolls = sync.pane_scrolls(start, VIEW);
        assert!(scrolls.iter().all(|&scroll| scroll == scrolls[0]), "{scrolls:?}");
        let [left, middle, right] = sync.pane_scrolls(60.0, VIEW);
        assert_eq!((middle - left, middle - right), (30.0, 20.0));
    }

    #[test]
    fn empty_documents_do_not_panic() {
        let sync = map(0, 0, vec![]);
        assert_eq!(sync.pane_scrolls(0.0, VIEW), [0.0, 0.0]);
        let sync = map(0, 5, vec![[0..0, 0..5]]);
        assert_eq!(sync.pane_scrolls(0.0, VIEW), [0.0, 0.0]);
    }
}
