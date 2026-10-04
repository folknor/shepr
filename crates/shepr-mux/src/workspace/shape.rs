//! The neutral form of a pane layout, with one value per pane.

use shepr_core::layout::{Direction, SplitRatio};

/// A pane layout with one `T` per pane: what capture writes a workspace into
/// and what restore builds one from. It names no pane identity of its own, so
/// a shape can hold saved data (restore) or live records (capture).
#[derive(Debug, Clone, PartialEq)]
pub enum Shape<T> {
    Pane(T),
    Split {
        direction: Direction,
        ratio: SplitRatio,
        first: Box<Shape<T>>,
        second: Box<Shape<T>>,
    },
}

impl<T> Shape<T> {
    /// Drops the leaves `keep` refuses, collapsing a split left with one
    /// child. `None` when no leaf is kept. `keep` sees the leaves in tree
    /// order.
    pub fn prune(self, keep: &mut impl FnMut(&T) -> bool) -> Option<Self> {
        match self {
            Self::Pane(leaf) => keep(&leaf).then_some(Self::Pane(leaf)),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let first = (*first).prune(keep);
                let second = (*second).prune(keep);
                match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        direction,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                    (None, None) => None,
                }
            }
        }
    }

    /// The same layout with each leaf mapped, in tree order.
    pub fn map<U>(self, f: &mut impl FnMut(T) -> U) -> Shape<U> {
        match self {
            Self::Pane(leaf) => Shape::Pane(f(leaf)),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let first = (*first).map(f);
                let second = (*second).map(f);
                Shape::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }
            }
        }
    }

    /// The first leaf in tree order. A shape always has one.
    pub fn first_leaf(&self) -> &T {
        match self {
            Self::Pane(leaf) => leaf,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    /// Leaves in tree order.
    pub fn leaves(&self) -> Vec<&T> {
        fn collect<'a, T>(shape: &'a Shape<T>, leaves: &mut Vec<&'a T>) {
            match shape {
                Shape::Pane(leaf) => leaves.push(leaf),
                Shape::Split { first, second, .. } => {
                    collect(first, leaves);
                    collect(second, leaves);
                }
            }
        }
        let mut leaves = Vec::new();
        collect(self, &mut leaves);
        leaves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(first: Shape<u32>, second: Shape<u32>) -> Shape<u32> {
        Shape::Split {
            direction: Direction::Horizontal,
            ratio: SplitRatio::EVEN,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn leaves_are_in_tree_order() {
        let shape = split(Shape::Pane(3), split(Shape::Pane(1), Shape::Pane(2)));
        assert_eq!(shape.leaves(), vec![&3, &1, &2]);
    }

    #[test]
    fn pruning_collapses_a_split_left_with_one_child() {
        let shape = split(Shape::Pane(11), Shape::Pane(12));
        let pruned = shape.prune(&mut |leaf| *leaf == 11);
        assert_eq!(pruned, Some(Shape::Pane(11)));
    }

    #[test]
    fn pruning_every_leaf_leaves_nothing() {
        let shape = split(Shape::Pane(1), Shape::Pane(2));
        assert_eq!(shape.prune(&mut |_| false), None);
    }

    #[test]
    fn mapping_keeps_the_layout_and_visits_leaves_in_order() {
        let shape = split(Shape::Pane(3), split(Shape::Pane(1), Shape::Pane(2)));
        let mut visited = Vec::new();
        let mapped = shape.map(&mut |leaf| {
            visited.push(leaf);
            leaf * 10
        });
        assert_eq!(visited, vec![3, 1, 2]);
        assert_eq!(mapped.leaves(), vec![&30, &10, &20]);
    }
}
