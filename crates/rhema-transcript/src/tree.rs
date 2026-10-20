//! The slot algebra: how ranges of the log are named and split.
//!
//! A range `(id, n)` is the `n` messages of the log from index `id` on.
//! `n = 1` is one message; a larger range splits into the halves
//! `(id, n/2)` and `(id + n/2, n - n/2)`, which always cover it. Any
//! `n` works — the tree a later compactor will grow is binary, but the
//! algebra does not need it. This module is the contract every fill
//! policy keeps: the lines change, the addresses never.

/// A range of the log: the `n` messages from `id` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// Index of the first message.
    pub id: usize,
    /// How many messages the range covers; at least 1.
    pub n: usize,
}

impl Range {
    /// The range's single message: `(id, 1)`.
    pub fn message(id: usize) -> Self {
        Self { id, n: 1 }
    }

    /// The two halves covering `self`. `n = 1` cannot split; the second
    /// half is empty and [`None`] stands in for it.
    pub fn halves(&self) -> (Range, Option<Range>) {
        if self.n <= 1 {
            return (*self, None);
        }
        let first = Self {
            id: self.id,
            n: self.n / 2,
        };
        let second = Self {
            id: self.id + first.n,
            n: self.n - first.n,
        };
        (first, Some(second))
    }
}

/// The cover of the log's old prefix `[0, cut)`: doubling blocks
/// anchored at the cut, walking back — sizes 1, 2, 4, 8 … — with the
/// oldest block taking the remainder. Oldest first, so a view lists
/// them in reading order. A prefix of `cut` messages costs `O(log cut)`
/// blocks at any age.
pub fn old_cover(cut: usize) -> Vec<Range> {
    let mut blocks = Vec::new();
    let mut end = cut;
    let mut size = 1;
    while end > 0 {
        let start = end.saturating_sub(size);
        blocks.push(Range {
            id: start,
            n: end - start,
        });
        end = start;
        size = size.saturating_mul(2);
    }
    blocks.reverse();
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover_ids(cut: usize) -> Vec<(usize, usize)> {
        old_cover(cut).into_iter().map(|r| (r.id, r.n)).collect()
    }

    #[test]
    fn halves_cover_their_range_at_every_n() {
        for n in 1..64 {
            let range = Range { id: 7, n };
            let (first, second) = range.halves();
            assert_eq!(first.id, 7);
            assert_eq!(first.n + second.map_or(0, |r| r.n), n);
            if n > 1 {
                let second = second.unwrap();
                assert!(first.n >= 1 && second.n >= 1);
                assert_eq!(first.n + second.n, n);
                assert_eq!(second.id, 7 + first.n);
            } else {
                assert!(second.is_none());
            }
        }
    }

    #[test]
    fn the_old_cover_tiles_the_prefix_without_gaps_or_overlap() {
        for cut in 0..128 {
            let cover = old_cover(cut);
            let mut at = 0;
            for range in &cover {
                assert_eq!(range.id, at, "cut {cut}");
                at += range.n;
            }
            assert_eq!(at, cut, "cut {cut}");
        }
    }

    #[test]
    fn the_old_cover_grows_toward_the_past() {
        // Sizes are non-decreasing from the cut backward: 1, 2, 4 …,
        // the remainder landing in the oldest block.
        assert_eq!(cover_ids(5), vec![(0, 2), (2, 2), (4, 1)]);
        assert_eq!(cover_ids(1), vec![(0, 1)]);
        assert_eq!(cover_ids(0), Vec::new());
        // 1000 walks back as 1, 2, 4 … 256, then the remainder 489 in
        // the oldest block: ten blocks for a thousand messages.
        let cover = cover_ids(1_000);
        assert_eq!(cover.len(), 10);
        assert_eq!(cover.first().copied(), Some((0, 489)));
        assert_eq!(cover.last().copied(), Some((999, 1)));
    }
}
