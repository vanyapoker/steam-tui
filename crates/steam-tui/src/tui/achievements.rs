//! The achievements viewer's state: sorting, filtering and selection. Sorting
//! keeps unknown values (no rarity, no date) last in either direction.
use std::cmp::Ordering;

use steam_core::achievements::{Achievement, Achievements};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AchSort {
    /// Unlocked first (newest first), then locked, commonest first.
    Status,
    Name,
    /// Commonest first.
    Rarity,
    /// Newest unlock first.
    Date,
}

impl AchSort {
    pub fn next(self) -> Self {
        match self {
            Self::Status => Self::Name,
            Self::Name => Self::Rarity,
            Self::Rarity => Self::Date,
            Self::Date => Self::Status,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Name => "name",
            Self::Rarity => "rarity",
            Self::Date => "date",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AchFilter {
    All,
    Unlocked,
    Locked,
}

impl AchFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Unlocked,
            Self::Unlocked => Self::Locked,
            Self::Locked => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unlocked => "unlocked",
            Self::Locked => "locked",
        }
    }

    fn keeps(self, item: &Achievement) -> bool {
        match self {
            Self::All => true,
            Self::Unlocked => item.unlocked,
            Self::Locked => !item.unlocked,
        }
    }
}

pub(super) struct AchievementsView {
    pub request: u64,
    pub app_id: u32,
    pub name: String,
    pub language: String,
    pub result: Option<Result<Achievements, String>>,
    pub sort: AchSort,
    pub reverse: bool,
    pub filter: AchFilter,
    /// Position in the visible (filtered, sorted) list.
    pub selected: usize,
    pub offset: usize,
}

/// Orders known values by `known`; `None` is last whichever way the list runs.
fn optional<T>(
    a: Option<T>,
    b: Option<T>,
    reverse: bool,
    known: impl Fn(T, T) -> Ordering,
) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            let order = known(a, b);
            if reverse { order.reverse() } else { order }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn by_name(a: &Achievement, b: &Achievement) -> Ordering {
    a.display_name
        .to_lowercase()
        .cmp(&b.display_name.to_lowercase())
        .then_with(|| a.name.cmp(&b.name))
}

fn commonest_first(a: Option<f32>, b: Option<f32>, reverse: bool) -> Ordering {
    optional(a, b, reverse, |a, b| b.total_cmp(&a))
}

impl AchievementsView {
    pub fn new(request: u64, app_id: u32, name: String, language: String) -> Self {
        Self {
            request,
            app_id,
            name,
            language,
            result: None,
            sort: AchSort::Status,
            reverse: false,
            filter: AchFilter::All,
            selected: 0,
            offset: 0,
        }
    }

    pub fn loaded(&self) -> Option<&Achievements> {
        self.result.as_ref()?.as_ref().ok()
    }

    /// The achievements shown: filtered, then sorted.
    pub fn visible(&self) -> Vec<&Achievement> {
        let Some(all) = self.loaded() else {
            return Vec::new();
        };
        let mut items: Vec<&Achievement> =
            all.items.iter().filter(|a| self.filter.keeps(a)).collect();
        let reverse = self.reverse;
        items.sort_by(|a, b| match self.sort {
            AchSort::Name => {
                let order = by_name(a, b);
                if reverse { order.reverse() } else { order }
            }
            AchSort::Rarity => {
                commonest_first(a.percent, b.percent, reverse).then_with(|| by_name(a, b))
            }
            AchSort::Date => optional(a.unlock_time, b.unlock_time, reverse, |a, b| b.cmp(&a))
                .then_with(|| by_name(a, b)),
            AchSort::Status => (!a.unlocked)
                .cmp(&!b.unlocked)
                .then_with(|| {
                    if a.unlocked {
                        optional(a.unlock_time, b.unlock_time, reverse, |a, b| b.cmp(&a))
                    } else {
                        commonest_first(a.percent, b.percent, reverse)
                    }
                })
                .then_with(|| by_name(a, b)),
        });
        items
    }

    pub fn selected_name(&self) -> Option<String> {
        self.visible().get(self.selected).map(|a| a.name.clone())
    }

    /// Re-applies the order after a change, keeping `name` selected when it is still listed.
    pub fn reselect(&mut self, name: Option<String>) {
        let visible = self.visible();
        self.selected = name
            .and_then(|name| visible.iter().position(|a| a.name == name))
            .unwrap_or(0)
            .min(visible.len().saturating_sub(1));
        self.offset = 0;
    }

    pub fn cycle_sort(&mut self) {
        let keep = self.selected_name();
        self.sort = self.sort.next();
        self.reselect(keep);
    }

    /// `1`-`4`: that sort, or its reverse when it is already active.
    pub fn select_sort(&mut self, digit: char) {
        let sort = match digit {
            '1' => AchSort::Status,
            '2' => AchSort::Name,
            '3' => AchSort::Rarity,
            _ => AchSort::Date,
        };
        if self.sort == sort {
            self.flip();
        } else {
            let keep = self.selected_name();
            self.sort = sort;
            self.reselect(keep);
        }
    }

    pub fn flip(&mut self) {
        let keep = self.selected_name();
        self.reverse = !self.reverse;
        self.reselect(keep);
    }

    pub fn cycle_filter(&mut self) {
        let keep = self.selected_name();
        self.filter = self.filter.next();
        self.reselect(keep);
    }

    pub fn step(&mut self, by: isize) {
        let last = self.visible().len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, unlocked: bool, time: Option<u32>, percent: Option<f32>) -> Achievement {
        Achievement {
            name: name.into(),
            display_name: name.into(),
            description: String::new(),
            hidden: false,
            unlocked,
            unlock_time: time,
            percent,
        }
    }

    fn view() -> AchievementsView {
        let mut view = AchievementsView::new(1, 10, "Game".into(), "english".into());
        view.result = Some(Ok(Achievements {
            items: vec![
                item("b", true, Some(200), Some(50.0)),
                item("a", true, Some(300), Some(5.0)),
                item("c", false, None, Some(20.0)),
                item("d", false, None, None),
                item("e", true, None, Some(1.0)),
            ],
            has_percentages: true,
        }));
        view
    }

    fn names(view: &AchievementsView) -> String {
        view.visible().iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn status_lists_newest_unlocks_then_locked_commonest_first() {
        // Unlocked: a(300), b(200), e(no date last); locked: c(20%), d(unknown last).
        assert_eq!(names(&view()), "abecd");
    }

    #[test]
    fn name_rarity_and_date_sorts_keep_unknowns_last_when_reversed() {
        let mut v = view();
        v.sort = AchSort::Name;
        assert_eq!(names(&v), "abcde");
        v.reverse = true;
        assert_eq!(names(&v), "edcba");
        v.sort = AchSort::Rarity;
        v.reverse = false;
        assert_eq!(names(&v), "bcaed"); // 50, 20, 5, 1, unknown
        v.reverse = true;
        assert_eq!(names(&v), "eacbd"); // rarest first, unknown still last
        v.sort = AchSort::Date;
        v.reverse = false;
        assert_eq!(names(&v), "abcde"); // 300, 200, then undated by name
        v.reverse = true;
        assert_eq!(names(&v), "bacde"); // oldest first, undated still last
    }

    #[test]
    fn filters_and_selection_follow_the_item() {
        let mut v = view();
        v.selected = 3; // "c"
        v.cycle_sort(); // name order: c is index 2
        assert_eq!(v.selected_name().as_deref(), Some("c"));
        v.cycle_filter(); // unlocked only: c disappears
        assert_eq!(names(&v), "abe");
        assert_eq!(v.selected, 0);
        v.cycle_filter();
        assert_eq!(names(&v), "cd");
        v.step(5);
        assert_eq!(v.selected, 1);
        v.step(-9);
        assert_eq!(v.selected, 0);
    }
}
