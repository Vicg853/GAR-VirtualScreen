//! Layout primitives shared by the host and viewer controls: one config per
//! row, label on the left, the fitting control on the right.
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::Button,
    menu::{DropdownMenu, PopupMenuItem},
    switch::Switch,
    tooltip::Tooltip,
};
use gpui_kit::{
    AnyElement, App, Div, ElementId, FontWeight, Role, SharedString, Stateful, TestSupportExt,
    Window, div, prelude::*, px,
};
use std::rc::Rc;

pub const LABEL_WIDTH: f32 = 132.;

/// Small uppercase heading above a card.
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(text.into().to_uppercase())
}

/// Heading with something right-aligned next to it (a pill or a button).
pub fn section_header(text: impl Into<SharedString>, trailing: impl IntoElement, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(section_label(text, cx))
        .child(trailing)
}

/// A labelled group: heading, then its card.
pub fn section(heading: impl IntoElement, body: impl IntoElement) -> Div {
    div().flex().flex_col().gap_2().child(heading).child(body)
}

/// Bordered group whose rows are separated by hairlines.
pub fn card(rows: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    let border = cx.theme().border;
    let mut card = div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(border)
        .bg(cx.theme().background)
        .overflow_hidden();
    for (index, row) in rows.into_iter().enumerate() {
        card = card.child(
            div()
                .when(index > 0, |row| row.border_t_1().border_color(border))
                .child(row),
        );
    }
    card
}

fn row_base() -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px(px(14.))
        .py(px(10.))
        .min_h(px(48.))
        .min_w_0()
}

fn key(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(13.))
        .text_color(cx.theme().foreground.opacity(0.85))
        .child(label.into())
}

/// Label column on the left, control filling the rest.
pub fn row(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> Div {
    row_base()
        .child(key(label, cx).w(px(LABEL_WIDTH)).flex_none())
        .child(div().flex_1().min_w_0().child(control))
}

/// Like [`row`], with an explanation under the control.
pub fn row_with_hint(
    label: impl Into<SharedString>,
    control: impl IntoElement,
    explanation: impl Into<SharedString>,
    cx: &App,
) -> Div {
    row_base()
        .items_start()
        .child(key(label, cx).w(px(LABEL_WIDTH)).flex_none().pt(px(6.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(control)
                .child(hint(explanation, cx)),
        )
}

/// Wide label (optionally explained) with a compact control on the right: toggles and buttons.
pub fn row_wide(
    label: impl Into<SharedString>,
    explanation: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    row_base()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(key(label, cx))
                .when_some(explanation, |column, text| column.child(hint(text, cx))),
        )
        .child(div().flex_none().child(control))
}

pub fn hint(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// Inline group: control, unit, control…
pub fn inline() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}

pub fn unit(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_none()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pill {
    Neutral,
    Live,
    Warn,
}

/// Status pill. `Live` is the filled one used for the state a row is in.
pub fn pill(text: impl Into<SharedString>, kind: Pill, cx: &App) -> Div {
    let theme = cx.theme();
    let (background, foreground) = match kind {
        Pill::Neutral => (theme.muted, theme.muted_foreground),
        Pill::Live => (theme.foreground, theme.background),
        Pill::Warn => (theme.warning.opacity(0.18), theme.warning),
    };
    div()
        .flex_none()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .bg(background)
        .text_color(foreground)
        .text_size(px(11.))
        .child(text.into())
}

/// Small monitor glyph. Dashed for temporary screens, dimmed for paused ones.
pub fn monitor_icon(dashed: bool, dim: bool, cx: &App) -> Div {
    let color = cx.theme().foreground;
    div()
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .when(dim, |icon| icon.opacity(0.5))
        .child(
            div()
                .w(px(32.))
                .h(px(22.))
                .rounded(px(3.))
                .border_2()
                .border_color(color)
                .when(dashed, |screen| screen.border_dashed()),
        )
        .child(div().w(px(12.)).h(px(2.)).mt(px(3.)).bg(color))
}

fn item_text(name: SharedString, detail: SharedString, cx: &App) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(name),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(detail),
        )
}

fn item_base() -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .min_w_0()
}

/// List row: optional leading glyph, name over detail, then trailing controls.
pub fn item(
    leading: Option<AnyElement>,
    name: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    trailing: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> Div {
    item_base()
        .children(leading)
        .child(item_text(name.into(), detail.into(), cx))
        .children(trailing)
}

/// Like [`item`], but the glyph and text open something when clicked. The
/// trailing controls stay outside the clickable area.
pub fn item_link(
    id: impl Into<ElementId>,
    leading: Option<AnyElement>,
    name: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    on_open: impl Fn(&mut Window, &mut App) + 'static,
    trailing: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> Div {
    let name: SharedString = name.into();
    item_base()
        .child(
            div()
                .id(id)
                .role(Role::Button)
                .aria_label(name.clone())
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(10.))
                .cursor_pointer()
                .rounded(px(6.))
                .hover(|row| row.opacity(0.7))
                .on_click(move |_, window, cx| {
                    on_open(window, cx);
                    window.refresh();
                })
                .children(leading)
                .child(item_text(name, detail.into(), cx))
                .test_support(),
        )
        .children(trailing)
}

/// Collapsible group header: title on the left, chevron on the right.
pub fn accordion_header(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let title: SharedString = title.into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title.clone())
        .aria_expanded(open)
        .flex()
        .items_center()
        .gap_2()
        .px(px(14.))
        .py(px(10.))
        .bg(cx.theme().muted)
        .cursor_pointer()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .on_click(move |_, window, cx| {
            on_toggle(window, cx);
            window.refresh();
        })
        .child(div().flex_1().min_w_0().truncate().child(title))
        .child(Icon::new(if open {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        }))
        .test_support()
}

/// One value of a segmented control or dropdown.
#[derive(Clone)]
pub struct Choice {
    pub label: SharedString,
    pub selected: bool,
    /// Why the value is handled by AirBridge but cannot be picked here.
    pub unavailable: Option<SharedString>,
}

impl Choice {
    pub fn new(label: impl Into<SharedString>, selected: bool) -> Self {
        Self {
            label: label.into(),
            selected,
            unavailable: None,
        }
    }
    pub fn unavailable(mut self, reason: Option<impl Into<SharedString>>) -> Self {
        self.unavailable = reason.map(Into::into);
        self
    }
}

/// Radio group drawn as joined segments. Unavailable values stay visible,
/// struck through, and explain themselves on hover.
pub fn segmented(
    id: impl Into<SharedString>,
    choices: Vec<Choice>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let id: SharedString = id.into();
    let theme = cx.theme();
    let on_select = Rc::new(on_select);
    let mut group = div()
        .flex()
        .flex_wrap()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted);
    for (index, choice) in choices.into_iter().enumerate() {
        let segment: Stateful<Div> = div()
            .id(ElementId::NamedInteger(id.clone(), index as u64))
            .role(Role::RadioButton)
            .aria_label(choice.label.clone())
            .aria_selected(choice.selected)
            .px(px(10.))
            .py(px(5.))
            .rounded(px(6.))
            .text_size(px(12.))
            .child(choice.label.clone());
        let segment = if let Some(reason) = choice.unavailable {
            segment
                .text_color(theme.muted_foreground.opacity(0.7))
                .line_through()
                .cursor_not_allowed()
                .tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
        } else if choice.selected {
            segment.bg(theme.foreground).text_color(theme.background)
        } else {
            let on_select = on_select.clone();
            segment
                .cursor_pointer()
                .text_color(theme.foreground.opacity(0.85))
                .hover(|segment| segment.bg(theme.background))
                .on_click(move |_, window, cx| {
                    on_select(index, window, cx);
                    window.refresh();
                })
        };
        group = group.child(segment.test_support());
    }
    div().flex().child(group)
}

/// Dropdown selector showing the current value.
pub fn select(
    id: impl Into<ElementId>,
    choices: Vec<Choice>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let current = choices
        .iter()
        .find(|choice| choice.selected)
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| "Select…".into());
    let on_select = Rc::new(on_select);
    Button::new(id)
        .label(current)
        .outline()
        .small()
        .dropdown_caret(true)
        .dropdown_menu(move |mut menu, _, _| {
            for (index, choice) in choices.iter().enumerate() {
                let on_select = on_select.clone();
                let label = match &choice.unavailable {
                    Some(reason) => format!("{} — {reason}", choice.label),
                    None => choice.label.to_string(),
                };
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(choice.selected)
                        .disabled(choice.unavailable.is_some())
                        .on_click(move |_, window, cx| {
                            on_select(index, window, cx);
                            window.refresh();
                        }),
                );
            }
            menu
        })
}

/// Controlled toggle; the owner writes the requested value.
pub fn toggle(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Switch {
    Switch::new(id)
        .checked(checked)
        .accessibility_label(label)
        .on_click(move |value, window, cx| {
            on_change(*value, window, cx);
            window.refresh();
        })
}

/// Compact bordered button used in title bars and list rows.
pub fn tool_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id).label(label).outline().small()
}

pub fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
) -> Button {
    let label: SharedString = label.into();
    Button::new(id)
        .icon(Icon::new(icon))
        .outline()
        .small()
        .accessibility_label(label.clone())
        .tooltip(label)
}

/// Back button, title and trailing status under the title bar.
pub fn crumb(
    back: impl IntoElement,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    trailing: Option<AnyElement>,
    cx: &App,
) -> Div {
    let detail: SharedString = detail.into();
    div()
        .flex()
        .items_center()
        .gap_2()
        .px(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(cx.theme().border)
        .text_size(px(13.))
        .child(back)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .truncate()
                        .child(title.into()),
                )
                .when(!detail.is_empty(), |title| {
                    title.child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(format!("· {detail}")),
                    )
                }),
        )
        .children(trailing)
}

/// Wraps a group that exists in the interface but has no engine behind it yet.
pub fn dimmed(content: impl IntoElement) -> Div {
    div().opacity(0.55).child(content)
}

/// Disabled-state helper for kit buttons built through this module.
pub fn disabled_if(button: Button, disabled: bool) -> Button {
    button.disabled(disabled)
}
