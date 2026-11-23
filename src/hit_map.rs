//! Where the last frame drew what, so a mouse event can be matched to it.

use ratatui::layout::{Position, Rect};

/// What a screen region belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    /// The current screen, above the footer.
    Main,
    /// The footer.
    Footer,
}

/// The regions of the last frame, in drawing order. Rebuilt on every frame.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    /// Forget the previous frame, keeping the allocation.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Record a region drawn above the ones recorded before it.
    pub fn push(&mut self, area: Rect, target: HitTarget) {
        self.regions.push((area, target));
    }

    /// The topmost target at a terminal cell.
    #[must_use]
    pub fn at(&self, column: u16, row: u16) -> Option<HitTarget> {
        let position = Position::new(column, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_region_drawn_last_wins() {
        let mut hits = HitMap::default();
        hits.push(Rect::new(0, 0, 10, 10), HitTarget::Main);
        hits.push(Rect::new(2, 2, 3, 3), HitTarget::Footer);

        assert_eq!(hits.at(0, 0), Some(HitTarget::Main));
        assert_eq!(hits.at(2, 2), Some(HitTarget::Footer));
        assert_eq!(hits.at(4, 4), Some(HitTarget::Footer));
        assert_eq!(hits.at(5, 5), Some(HitTarget::Main));
    }

    #[test]
    fn outside_every_region_is_nothing() {
        let mut hits = HitMap::default();
        hits.push(Rect::new(1, 1, 2, 2), HitTarget::Main);

        assert_eq!(hits.at(0, 0), None);
        assert_eq!(hits.at(3, 1), None);
        hits.clear();
        assert_eq!(hits.at(1, 1), None);
    }
}
