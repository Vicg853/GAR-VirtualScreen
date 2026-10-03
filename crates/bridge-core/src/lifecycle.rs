//! Shared navigation rules for the host and viewer control widgets.
use crate::protocol::ScreenId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenView {
    #[default]
    List,
    Details,
}

#[derive(Clone, Debug, Default)]
pub struct ScreenNavigation {
    pub selected: Option<ScreenId>,
    pub view: ScreenView,
}

impl ScreenNavigation {
    pub fn open(&mut self, screen_id: ScreenId) {
        self.selected = Some(screen_id);
        self.view = ScreenView::Details;
    }

    pub fn back(&mut self) {
        self.view = ScreenView::List;
    }

    pub fn remove(&mut self, screen_id: ScreenId, fallback: Option<ScreenId>) {
        if self.selected == Some(screen_id) {
            self.selected = fallback;
            self.back();
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_detail_and_removal_flows_preserve_valid_selection() {
        let mut nav = ScreenNavigation::default();
        nav.open(3);
        nav.back();
        assert_eq!(nav.selected, Some(3));
        assert_eq!(nav.view, ScreenView::List);
        nav.open(3);
        nav.remove(2, Some(3));
        assert_eq!(nav.view, ScreenView::Details);
        nav.remove(3, Some(4));
        assert_eq!(nav.selected, Some(4));
        assert_eq!(nav.view, ScreenView::List);
        nav.open(4);
        nav.remove(4, None);
        assert_eq!(nav.selected, None);
        assert_eq!(nav.view, ScreenView::List);
        nav.open(5);
        nav.clear();
        assert_eq!(nav.selected, None);
        assert_eq!(nav.view, ScreenView::List);
    }
}
