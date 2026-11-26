//! A position in the room refers to a board, topic and post by identity.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum BoardLayer {
    #[default]
    Boards,
    Topics,
    Posts,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Place {
    pub layer: BoardLayer,
    pub board: Option<String>,
    pub topic: Option<String>,
    pub post: Option<usize>,
}

#[derive(Default)]
pub(super) struct Trail {
    pub here: Place,
    before_guide: Option<Place>,
}

impl Trail {
    pub fn visit_guide(&mut self, board: &str, topic: &str) {
        if self.before_guide.is_none() {
            self.before_guide = Some(self.here.clone());
        }
        self.here.board = Some(board.into());
        self.here.topic = Some(topic.into());
        self.here.post = None;
        self.here.layer = BoardLayer::Posts;
    }

    pub fn back(&mut self) -> bool {
        if let Some(place) = self.before_guide.take() {
            self.here = place;
            return true;
        }
        self.here.layer = match self.here.layer {
            BoardLayer::Posts => BoardLayer::Topics,
            BoardLayer::Topics => BoardLayer::Boards,
            BoardLayer::Boards => return false,
        };
        self.here.post = None;
        true
    }
}

pub(super) fn step<T: Copy + PartialEq>(items: &[T], current: Option<T>, delta: isize) -> Option<T> {
    let last = items.len().checked_sub(1)?;
    let index = items.iter().position(|id| Some(*id) == current).unwrap_or(0);
    Some(items[index.saturating_add_signed(delta).min(last)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_from_the_guide_restores_the_post_being_read() {
        let home = Place {
            layer: BoardLayer::Posts, board: Some("band".into()),
            topic: Some("handoff".into()), post: Some(17),
        };
        let mut trail = Trail { here: home.clone(), ..Default::default() };
        trail.visit_guide("chomosuke/memory", "note");
        trail.visit_guide("chomosuke/memory", "note");
        assert_eq!(trail.here.post, None);
        assert!(trail.back());
        assert_eq!(trail.here, home);
        assert!(trail.back());
        assert_eq!(trail.here.layer, BoardLayer::Topics);
        assert!(trail.back());
        assert_eq!(trail.here.layer, BoardLayer::Boards);
        assert!(!trail.back());
    }

    #[test]
    fn new_activity_above_the_cursor_preserves_its_identity_and_neighbours() {
        let reordered = ["new", "handoff", "sound"];
        assert_eq!(step(&reordered, Some("handoff"), 0), Some("handoff"));
        assert_eq!(step(&reordered, Some("handoff"), 1), Some("sound"));
        assert_eq!(step(&reordered, Some("sound"), 1), Some("sound"));
        assert_eq!(step(&reordered, Some("gone"), 0), Some("new"));
        assert_eq!(step::<&str>(&[], None, 1), None);
    }
}
