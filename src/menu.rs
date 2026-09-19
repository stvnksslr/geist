//! The terminal's right-click menu, as data: which items appear, in what
//! order, and which are enabled. Mirrors the macOS app's surface context menu
//! (`SurfaceView_AppKit.swift`, `menu(for:)`); the app only renders it.

use crate::command::Action;

/// A menu entry's identity. Most map onto a keybind [`Action`], so the menu
/// and the bindings run one implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuId {
    Copy,
    Paste,
    CopyUrl,
    SplitRight,
    SplitLeft,
    SplitDown,
    SplitUp,
    SelectAll,
    ResetTerminal,
    ToggleInspector,
    ToggleReadonly,
    ChangeTabTitle,
    ChangeTerminalTitle,
}

impl MenuId {
    /// The action this item runs through `execute_action`, or `None` for the
    /// items the app handles on the right-clicked pane directly.
    pub fn action(self) -> Option<Action> {
        Some(match self {
            MenuId::Copy | MenuId::Paste | MenuId::CopyUrl | MenuId::SelectAll => return None,
            MenuId::SplitRight => Action::SplitRight,
            MenuId::SplitLeft => Action::SplitLeft,
            MenuId::SplitDown => Action::SplitDown,
            MenuId::SplitUp => Action::SplitUp,
            MenuId::ResetTerminal => Action::ResetTerminal,
            MenuId::ToggleInspector => {
                Action::Inspector(crate::inspector::InspectorMode::Toggle)
            }
            MenuId::ToggleReadonly => Action::ToggleReadonly,
            MenuId::ChangeTabTitle => Action::PromptTabTitle,
            MenuId::ChangeTerminalTitle => Action::PromptSurfaceTitle,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Separator,
    Item {
        id: MenuId,
        label: &'static str,
        enabled: bool,
        /// Drawn with a check mark (a toggle that is on).
        checked: bool,
    },
}

fn item(id: MenuId, label: &'static str) -> MenuItem {
    MenuItem::Item { id, label, enabled: true, checked: false }
}

/// The menu for a pane. `has_selection` enables Copy; `on_link` adds Copy URL
/// (upstream only offers it when the click landed on a link); `readonly`
/// checks the Read-only toggle.
pub fn terminal_menu(has_selection: bool, on_link: bool, readonly: bool) -> Vec<MenuItem> {
    let mut m = Vec::with_capacity(20);
    if on_link {
        m.push(item(MenuId::CopyUrl, "Copy URL"));
        m.push(MenuItem::Separator);
    }
    m.push(MenuItem::Item {
        id: MenuId::Copy,
        label: "Copy",
        enabled: has_selection,
        checked: false,
    });
    m.push(item(MenuId::Paste, "Paste"));
    m.push(MenuItem::Separator);
    m.push(item(MenuId::SplitRight, "Split Right"));
    m.push(item(MenuId::SplitLeft, "Split Left"));
    m.push(item(MenuId::SplitDown, "Split Down"));
    m.push(item(MenuId::SplitUp, "Split Up"));
    m.push(MenuItem::Separator);
    m.push(item(MenuId::SelectAll, "Select All"));
    m.push(item(MenuId::ResetTerminal, "Reset Terminal"));
    m.push(item(MenuId::ToggleInspector, "Toggle Terminal Inspector"));
    m.push(MenuItem::Item {
        id: MenuId::ToggleReadonly,
        label: "Terminal Read-only",
        enabled: true,
        checked: readonly,
    });
    m.push(MenuItem::Separator);
    m.push(item(MenuId::ChangeTabTitle, "Change Tab Title…"));
    m.push(item(MenuId::ChangeTerminalTitle, "Change Terminal Title…"));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(m: &[MenuItem]) -> Vec<MenuId> {
        m.iter()
            .filter_map(|i| match i {
                MenuItem::Item { id, .. } => Some(*id),
                MenuItem::Separator => None,
            })
            .collect()
    }

    #[test]
    fn the_full_menu_in_order() {
        use MenuId::*;
        assert_eq!(
            ids(&terminal_menu(true, false, false)),
            vec![
                Copy,
                Paste,
                SplitRight,
                SplitLeft,
                SplitDown,
                SplitUp,
                SelectAll,
                ResetTerminal,
                ToggleInspector,
                ToggleReadonly,
                ChangeTabTitle,
                ChangeTerminalTitle,
            ]
        );
    }

    #[test]
    fn copy_url_only_on_a_link_and_first() {
        assert!(!ids(&terminal_menu(false, false, false)).contains(&MenuId::CopyUrl));
        assert_eq!(ids(&terminal_menu(false, true, false))[0], MenuId::CopyUrl);
    }

    #[test]
    fn copy_needs_a_selection_and_readonly_is_checked() {
        let find = |m: &[MenuItem], want: MenuId| {
            m.iter()
                .find_map(|i| match i {
                    MenuItem::Item { id, enabled, checked, .. } if *id == want => {
                        Some((*enabled, *checked))
                    }
                    _ => None,
                })
                .unwrap()
        };
        let m = terminal_menu(false, false, true);
        assert_eq!(find(&m, MenuId::Copy), (false, false));
        assert_eq!(find(&m, MenuId::ToggleReadonly), (true, true));
        let m = terminal_menu(true, false, false);
        assert_eq!(find(&m, MenuId::Copy), (true, false));
        assert_eq!(find(&m, MenuId::ToggleReadonly), (true, false));
    }

    #[test]
    fn items_route_through_the_keybind_actions() {
        assert_eq!(MenuId::SplitLeft.action(), Some(Action::SplitLeft));
        assert_eq!(MenuId::ChangeTerminalTitle.action(), Some(Action::PromptSurfaceTitle));
        assert_eq!(MenuId::ChangeTabTitle.action(), Some(Action::PromptTabTitle));
        assert_eq!(MenuId::Copy.action(), None, "copy acts on the clicked pane directly");
    }
}
