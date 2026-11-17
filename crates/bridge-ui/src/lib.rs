//! Shared controls presentation. Native video resources never enter this crate.
pub mod fields;
pub mod kit;
pub mod logs;

use bridge_core::telemetry::{Stage, TimingView};
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, Sizable, TitleBar,
    button::{Button, ButtonVariants},
    table::{Column, DataTable, TableDelegate, TableState},
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Entity, Global, IntoElement, MouseButton,
    Pixels, Render, ResizeEdge, SharedString, Size, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub const TIMING_HELP: &str = "Start = time since capture (cumulative; do not add columns). Duration = time spent in that stage. S/D in frame cells use the same definitions. Estimated timings include clock uncertainty. GPU completion is not monitor presentation.";

#[derive(Clone, Copy)]
pub enum InspectionAction {
    Frames,
    Averages,
    Freeze,
    Burst,
    Close,
}

/// Snapshot values only: no network reads, GPU ownership, or domain mutation in rendering.
pub struct InspectionData {
    title: String,
    rows: Vec<Vec<String>>,
    lines: Vec<String>,
    mode: TimingView,
    frozen: bool,
}
impl InspectionData {
    pub fn new(
        title: String,
        rows: Vec<Vec<String>>,
        lines: Vec<String>,
        mode: TimingView,
        frozen: bool,
    ) -> Self {
        Self {
            title,
            rows,
            lines,
            mode,
            frozen,
        }
    }
}

#[derive(Clone)]
pub struct InspectionSource {
    read: Arc<dyn Fn() -> Option<InspectionData> + Send + Sync>,
    action: Arc<dyn Fn(InspectionAction) + Send + Sync>,
}
impl InspectionSource {
    /// Release the domain diagnostics subscription, including window-open rollback.
    pub fn close(&self) {
        (self.action)(InspectionAction::Close);
    }
    pub fn new(
        read: impl Fn() -> Option<InspectionData> + Send + Sync + 'static,
        action: impl Fn(InspectionAction) + Send + Sync + 'static,
    ) -> Self {
        Self {
            read: Arc::new(read),
            action: Arc::new(action),
        }
    }
}

#[derive(Default)]
struct AuxiliaryWindows(HashMap<String, AnyWindowHandle>);
impl Global for AuxiliaryWindows {}

/// Open a secondary window (logs, settings) once per key; asking again focuses it.
pub fn open_auxiliary<V: Render + 'static>(
    key: String,
    window_size: Size<Pixels>,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
) -> anyhow::Result<()> {
    if !cx.has_global::<AuxiliaryWindows>() {
        cx.set_global(AuxiliaryWindows::default());
        cx.on_window_closed(|cx, closed_id| {
            cx.global_mut::<AuxiliaryWindows>()
                .0
                .retain(|_, handle| handle.window_id() != closed_id);
        })
        .detach();
    }
    if let Some(handle) = cx.global::<AuxiliaryWindows>().0.get(&key).copied()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Ok(());
    }
    let bounds = Bounds::centered(None, window_size, cx);
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(360.), px(240.))),
            ..TitleBar::window_options()
        },
        cx,
        build,
    )?;
    cx.global_mut::<AuxiliaryWindows>().0.insert(key, handle);
    Ok(())
}

/// Close the secondary windows of something that no longer exists (a removed screen).
pub fn close_auxiliary(prefix: &str, cx: &mut App) {
    if !cx.has_global::<AuxiliaryWindows>() {
        return;
    }
    let handles = cx
        .global::<AuxiliaryWindows>()
        .0
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .map(|(_, handle)| *handle)
        .collect::<Vec<_>>();
    for handle in handles {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// Client-decorated windows have no native resize border; emulate one.
pub fn resize_from_border(event: &gpui_kit::MouseDownEvent, window: &mut Window) {
    let point = event.position;
    let size = window.window_bounds().get_bounds().size;
    let border = px(7.0);
    let left = point.x < border;
    let right = point.x > size.width - border;
    let top = point.y < border;
    let bottom = point.y > size.height - border;
    let edge = match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (_, true, true, _) => Some(ResizeEdge::TopRight),
        (true, _, _, true) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Left),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, true, _) => Some(ResizeEdge::Top),
        (_, _, _, true) => Some(ResizeEdge::Bottom),
        _ => None,
    };
    if let Some(edge) = edge {
        window.start_window_resize(edge);
    }
}

/// Main controls window: title bar with its right-aligned buttons, an optional
/// strip under it, then the scrolling page.
pub fn shell(
    title: impl Into<SharedString>,
    actions: Vec<gpui_kit::AnyElement>,
    strip: Option<gpui_kit::AnyElement>,
    page: impl IntoElement,
    cx: &App,
) -> gpui_kit::Div {
    div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            resize_from_border(event, window)
        })
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .text_color(cx.theme().foreground)
        .text_size(px(14.))
        .flex()
        .flex_col()
        .child(
            TitleBar::new().child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .pr_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(title.into()),
                    )
                    .children(actions),
            ),
        )
        .children(strip)
        .child(
            div()
                .id("page-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(18.))
                .child(page),
        )
}

struct InspectionWindow {
    handle: AnyWindowHandle,
    source: InspectionSource,
}
#[derive(Default)]
struct InspectionWindows(HashMap<String, InspectionWindow>);
impl Global for InspectionWindows {}

/// Reuse an inspector for the same domain screen; failed window handles are replaced.
pub fn open_inspector(key: String, source: InspectionSource, cx: &mut App) -> anyhow::Result<()> {
    if !cx.has_global::<InspectionWindows>() {
        cx.set_global(InspectionWindows::default());
        cx.on_window_closed(|cx, closed_id| {
            if cx.has_global::<InspectionWindows>() {
                let sources = {
                    let windows = &mut cx.global_mut::<InspectionWindows>().0;
                    let keys = windows
                        .iter()
                        .filter(|(_, entry)| entry.handle.window_id() == closed_id)
                        .map(|(key, _)| key.clone())
                        .collect::<Vec<_>>();
                    keys.into_iter()
                        .filter_map(|key| windows.remove(&key))
                        .map(|entry| entry.source)
                        .collect::<Vec<_>>()
                };
                for source in sources {
                    source.close();
                }
            }
        })
        .detach();
    }
    if let Some(handle) = cx
        .global::<InspectionWindows>()
        .0
        .get(&key)
        .map(|entry| entry.handle)
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Ok(());
    }
    let bounds = Bounds::centered(None, size(px(1100.), px(680.)), cx);
    let close_source = source.clone();
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(620.), px(400.))),
            ..TitleBar::window_options()
        },
        cx,
        move |window, cx| {
            window.set_window_title("AirBridge · Stats");
            let view = cx.new(|cx| Inspector::new(source, window, cx));
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    if weak.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            view
        },
    )?;
    cx.global_mut::<InspectionWindows>().0.insert(
        key,
        InspectionWindow {
            handle,
            source: close_source,
        },
    );
    Ok(())
}

struct TimingRows {
    headings: Vec<String>,
    rows: Vec<Vec<String>>,
}
impl TimingRows {
    fn new(mut values: Vec<Vec<String>>) -> Self {
        let headings = if values.is_empty() {
            vec!["Stage".into()]
        } else {
            values.remove(0)
        };
        Self {
            headings,
            rows: values,
        }
    }
}
impl TableDelegate for TimingRows {
    fn columns_count(&self, _: &App) -> usize {
        self.headings.len()
    }
    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }
    fn column(&self, index: usize, _: &App) -> Column {
        // Column widths are physical table measurements, maintained by TableState after resizing.
        let column = Column::new(self.headings[index].clone(), self.headings[index].clone())
            .width(if index == 0 { 225. } else { 205. })
            .movable(false);
        if index == 0 {
            column.fixed_left()
        } else if self.headings.first().is_some_and(|h| h == "Stage") && index <= 3 {
            column.text_right()
        } else {
            column
        }
    }
    fn render_tr(
        &mut self,
        row: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div().id(SharedString::from(format!("timing:{}", self.rows[row][0])))
    }
    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().text_sm().child(
            self.rows
                .get(row)
                .and_then(|r| r.get(column))
                .cloned()
                .unwrap_or_default(),
        )
    }
}

fn filtered_timing_rows(data: Option<&InspectionData>, gpu_only: bool) -> Vec<Vec<String>> {
    let Some(data) = data else {
        return Vec::new();
    };
    if !gpu_only {
        return data.rows.clone();
    }
    let stages = [
        Stage::DecodeQueue,
        Stage::Decode,
        Stage::RenderQueue,
        Stage::Upload,
        Stage::Submit,
        Stage::GpuComplete,
        Stage::PresentSubmit,
        Stage::Presentation,
        Stage::GpuInput,
        Stage::DecodeSubmit,
        Stage::SurfaceAcquire,
        Stage::GpuRender,
        Stage::SoftwareDecode,
        Stage::SoftwareUpload,
    ];
    let labels = stages.map(Stage::label);
    match data.mode {
        TimingView::Averages => data
            .rows
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                *index == 0 || row.first().is_some_and(|s| labels.contains(&s.as_str()))
            })
            .map(|(_, row)| row.clone())
            .collect(),
        TimingView::Frames => {
            let Some(headings) = data.rows.first() else {
                return Vec::new();
            };
            let indices = headings
                .iter()
                .enumerate()
                .filter(|(index, heading)| {
                    *index == 0
                        || labels.contains(&heading.as_str())
                        || heading.as_str() == "Summary"
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            data.rows
                .iter()
                .map(|row| {
                    indices
                        .iter()
                        .map(|index| row.get(*index).cloned().unwrap_or_default())
                        .collect()
                })
                .collect()
        }
    }
}

struct Inspector {
    source: InspectionSource,
    data: Option<InspectionData>,
    table: Entity<TableState<TimingRows>>,
    gpu_only: bool,
}
impl Inspector {
    fn new(source: InspectionSource, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let data = (source.read)();
        let rows = TimingRows::new(data.as_ref().map(|d| d.rows.clone()).unwrap_or_default());
        let table = cx.new(|cx| {
            TableState::new(rows, window, cx)
                .col_movable(false)
                .row_header(false)
        });
        Self {
            source,
            data,
            table,
            gpu_only: false,
        }
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let next_data = (self.source.read)();
        let source_changed = self.data.as_ref().map(|data| &data.title)
            != next_data.as_ref().map(|data| &data.title);
        self.data = next_data;
        let mut next = Some(TimingRows::new(filtered_timing_rows(
            self.data.as_ref(),
            self.gpu_only,
        )));
        self.table.update(cx, |table, cx| {
            let next = next.take().unwrap();
            let columns_changed = table.delegate().headings != next.headings;
            let rows_changed = table.delegate().rows != next.rows;
            let selected = (!source_changed)
                .then(|| table.selected_row())
                .flatten()
                .and_then(|i| table.delegate().rows.get(i))
                .and_then(|r| r.first())
                .cloned();
            *table.delegate_mut() = next;
            if columns_changed {
                table.refresh(cx);
            }
            // Keep selection attached to a frame/stage, rather than a changing row index.
            if rows_changed || source_changed {
                if let Some(index) = selected.and_then(|key| {
                    table
                        .delegate()
                        .rows
                        .iter()
                        .position(|r| r.first() == Some(&key))
                }) {
                    if table.selected_row() != Some(index) {
                        table.set_selected_row(index, cx);
                    }
                } else {
                    table.clear_selection(cx);
                }
            }
            cx.notify();
        });
        cx.notify();
    }
    fn action(&mut self, action: InspectionAction, cx: &mut Context<Self>) {
        (self.source.action)(action);
        self.refresh(cx);
    }
}
impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.data.as_ref().map(|d| d.mode);
        let frozen = self.data.as_ref().is_some_and(|d| d.frozen);
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(TitleBar::new().child("AirBridge · Stats"))
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        self.data
                            .as_ref()
                            .map(|d| d.title.clone())
                            .unwrap_or_else(|| {
                                "Stats closed or screen removed. Reopen Stats from the screen view."
                                    .into()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("inspector-frames")
                                    .label("Frame rows")
                                    .outline()
                                    .small()
                                    .selected(mode == Some(TimingView::Frames))
                                    .disabled(self.data.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action(InspectionAction::Frames, cx)
                                    })),
                            )
                            .child(
                                Button::new("inspector-averages")
                                    .label("Stage averages")
                                    .outline()
                                    .small()
                                    .selected(mode == Some(TimingView::Averages))
                                    .disabled(self.data.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action(InspectionAction::Averages, cx)
                                    })),
                            )
                            .child(
                                Button::new("inspector-gpu-filter")
                                    .label(if self.gpu_only {
                                        "All stages"
                                    } else {
                                        "GPU / display stages"
                                    })
                                    .outline()
                                    .small()
                                    .selected(self.gpu_only)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.gpu_only = !this.gpu_only;
                                        this.refresh(cx);
                                    })),
                            )
                            .child(
                                Button::new("inspector-freeze")
                                    .label(if frozen { "Resume" } else { "Freeze" })
                                    .outline()
                                    .small()
                                    .disabled(self.data.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action(InspectionAction::Freeze, cx)
                                    })),
                            )
                            .child(
                                Button::new("inspector-burst")
                                    .label("Capture next 20")
                                    .primary()
                                    .small()
                                    .disabled(self.data.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action(InspectionAction::Burst, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(TIMING_HELP),
                    )
                    .children(
                        self.data
                            .as_ref()
                            .map(|d| d.lines.clone())
                            .unwrap_or_default()
                            .into_iter()
                            .map(|line| {
                                div()
                                    .text_sm()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(line)
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .p_3()
                    .child(DataTable::new(&self.table).small()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_table_and_bounded_snapshot_rows() {
        let empty = TimingRows::new(vec![]);
        assert_eq!(empty.headings, ["Stage"]);
        assert!(empty.rows.is_empty());
        let rows = TimingRows::new(vec![
            vec!["Frame".into(), "Decode".into()],
            vec!["42".into(), "S: +70ms D: 2ms".into()],
        ]);
        assert_eq!(rows.headings.len(), 2);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], "42");
    }
    #[test]
    fn gpu_filter_preserves_identity_summary_and_unavailable_stages() {
        let rows = vec![
            vec![
                "Frame".into(),
                Stage::Encode.label().into(),
                Stage::GpuRender.label().into(),
                "Summary".into(),
            ],
            vec![
                "142".into(),
                "2ms".into(),
                "Unavailable".into(),
                "Incomplete".into(),
            ],
        ];
        let mut data =
            InspectionData::new("Screen".into(), rows, vec![], TimingView::Frames, false);
        let filtered = filtered_timing_rows(Some(&data), true);
        assert_eq!(filtered[1], ["142", "Unavailable", "Incomplete"]);
        data.mode = TimingView::Averages;
        data.rows = vec![
            vec!["Stage".into()],
            vec![Stage::Encode.label().into()],
            vec![Stage::Presentation.label().into()],
        ];
        assert_eq!(filtered_timing_rows(Some(&data), true).len(), 2);
        assert!(filtered_timing_rows(None, true).is_empty());
    }

    #[gpui_kit::test]
    fn inspectors_reuse_windows_and_release_closed_handles(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        use std::sync::atomic::{AtomicUsize, Ordering};
        let closes = Arc::new(AtomicUsize::new(0));
        let observed = closes.clone();
        let source = InspectionSource::new(
            || None,
            move |action| {
                if matches!(action, InspectionAction::Close) {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
            },
        );
        cx.update(|cx| {
            open_inspector("screen:1".into(), source.clone(), cx).unwrap();
            let first = cx.global::<InspectionWindows>().0["screen:1"].handle;
            open_inspector("screen:1".into(), source, cx).unwrap();
            assert_eq!(
                cx.global::<InspectionWindows>().0["screen:1"]
                    .handle
                    .window_id(),
                first.window_id()
            );
            assert_eq!(cx.global::<InspectionWindows>().0.len(), 1);
            first
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
        });
        cx.read(|cx| assert!(cx.global::<InspectionWindows>().0.is_empty()));
        assert_eq!(
            closes.load(Ordering::Relaxed),
            1,
            "closing releases diagnostics exactly once"
        );
    }
    #[gpui_kit::test]
    fn inspector_controls_keep_state_and_close_source_safely(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let frozen = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicUsize::new(0));
        let available = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicUsize::new(1));
        let read_generation = generation.clone();
        let read_frozen = frozen.clone();
        let read_mode = mode.clone();
        let read_available = available.clone();
        let write_frozen = frozen.clone();
        let write_mode = mode.clone();
        let source = InspectionSource::new(
            move || {
                if !read_available.load(Ordering::Relaxed) {
                    return None;
                }
                Some(InspectionData::new(
                    format!(
                        "Screen 42 · generation {}",
                        read_generation.load(Ordering::Relaxed)
                    ),
                    vec![
                        vec!["Stage".into(), "Duration".into()],
                        vec!["GPU render".into(), "2 ms".into()],
                    ],
                    vec![],
                    if read_mode.load(Ordering::Relaxed) == 0 {
                        TimingView::Frames
                    } else {
                        TimingView::Averages
                    },
                    read_frozen.load(Ordering::Relaxed),
                ))
            },
            move |action| match action {
                InspectionAction::Averages => {
                    write_mode.store(1, Ordering::Relaxed);
                }
                InspectionAction::Frames => {
                    write_mode.store(0, Ordering::Relaxed);
                }
                InspectionAction::Freeze => {
                    write_frozen.fetch_xor(true, Ordering::Relaxed);
                }
                InspectionAction::Burst | InspectionAction::Close => {}
            },
        );
        cx.update(gpui_kit::init);
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Inspector::new(source, window, cx))
            })
            .unwrap()
        });
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                window.click("inspector-freeze", cx);
                window.click("inspector-averages", cx);
            })
            .unwrap();
        assert!(frozen.load(Ordering::Relaxed));
        assert_eq!(mode.load(Ordering::Relaxed), 1);
        cx.update(|cx| {
            view.update(cx, |this, cx| {
                this.table
                    .update(cx, |table, cx| table.set_selected_row(0, cx));
                this.refresh(cx);
                assert_eq!(this.table.read(cx).selected_row(), Some(0));
            })
        });
        generation.store(2, Ordering::Relaxed);
        cx.update(|cx| {
            view.update(cx, |this, cx| {
                this.refresh(cx);
                assert_eq!(
                    this.table.read(cx).selected_row(),
                    None,
                    "a replacement generation must not inherit frame selection"
                );
            })
        });
        available.store(false, Ordering::Relaxed);
        cx.update(|cx| {
            view.update(cx, |this, cx| {
                this.refresh(cx);
                assert!(this.data.is_none());
                assert!(this.table.read(cx).delegate().rows.is_empty());
            })
        });
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                window.click("inspector-freeze", cx);
            })
            .unwrap();
        assert!(
            frozen.load(Ordering::Relaxed),
            "disabled action cannot mutate a closed stream"
        );
    }
}
