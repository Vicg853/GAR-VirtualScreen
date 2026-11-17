//! Per-screen log window. Reads bounded snapshots; never touches the stream.
use crate::{fields::Fields, kit, open_auxiliary};
use bridge_core::diagnostics::Diagnostics;
use gpui_kit::component::{ActiveTheme, Sizable, TitleBar, input::Input};
use gpui_kit::{
    App, AppContext, ClipboardItem, Context, IntoElement, Render, ScrollHandle, Window, div,
    prelude::*, px, size,
};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    All,
    Info,
    Warn,
    Error,
}

impl Level {
    const ALL: [Level; 4] = [Level::All, Level::Info, Level::Warn, Level::Error];
    fn label(self) -> &'static str {
        match self {
            Level::All => "All",
            Level::Info => "Info",
            Level::Warn => "Warn",
            Level::Error => "Error",
        }
    }
}

/// Log lines carry no level of their own; classify by what they report.
fn level_of(line: &str) -> Level {
    let line = line.to_lowercase();
    if [
        "error",
        "failed",
        "failure",
        "denied",
        "cannot ",
        "could not",
        "panick",
    ]
    .iter()
    .any(|word| line.contains(word))
    {
        Level::Error
    } else if [
        "warn",
        "timeout",
        "timed out",
        "lost",
        "dropped",
        "resynchron",
        "unavailable",
        "stalled",
        "os changed",
        "retry",
    ]
    .iter()
    .any(|word| line.contains(word))
    {
        Level::Warn
    } else {
        Level::Info
    }
}

fn shown(line: &str, level: Level, filter: &str) -> bool {
    (level == Level::All || level_of(line) == level)
        && (filter.is_empty() || line.to_lowercase().contains(&filter.to_lowercase()))
}

struct LogsView {
    title: String,
    diagnostics: Arc<Diagnostics>,
    lines: Vec<String>,
    level: Level,
    filter: String,
    auto_scroll: bool,
    scroll: ScrollHandle,
    fields: Fields,
}

impl LogsView {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let lines = self.diagnostics.recent();
        if lines != self.lines {
            self.lines = lines;
            if self.auto_scroll {
                self.scroll.scroll_to_bottom();
            }
            cx.notify();
        }
    }
    fn visible(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter(|line| shown(line, self.level, &self.filter))
            .cloned()
            .collect()
    }
}

impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let filter = self.fields.input(
            "filter",
            &self.filter.clone(),
            crate::fields::InputSpec {
                placeholder: "Filter",
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, text, _, _, _| this.filter = text,
        );
        let levels = Level::ALL
            .iter()
            .map(|level| kit::Choice::new(level.label(), *level == self.level))
            .collect();
        let level_view = view.clone();
        let copy_text = self.visible().join("\n");
        let path = self.diagnostics.path().map(|path| path.to_path_buf());
        let reveal = path.clone();
        let auto_view = view.clone();
        let theme = cx.theme();
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
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
                                .text_size(px(13.))
                                .child(self.title.clone()),
                        )
                        .child(kit::segmented(
                            "log-level",
                            levels,
                            move |index, _, cx| {
                                let _ = level_view.update(cx, |this, cx| {
                                    this.level = Level::ALL[index];
                                    cx.notify();
                                });
                            },
                            cx,
                        ))
                        .child(div().w(px(140.)).child(Input::new(&filter).small()))
                        .child(
                            kit::tool_button("log-copy", "Copy").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            }),
                        )
                        .child(
                            kit::disabled_if(
                                kit::tool_button("log-reveal", "Reveal file"),
                                reveal.is_none(),
                            )
                            .on_click(move |_, _, _| {
                                if let Some(path) = &reveal {
                                    reveal_file(path);
                                }
                            }),
                        ),
                ),
            )
            .child(
                div()
                    .id("log-lines")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(16.))
                    .py(px(12.))
                    .bg(theme.muted.opacity(0.4))
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(12.))
                    .children(self.visible().into_iter().map(|line| {
                        let color = match level_of(&line) {
                            Level::Error => theme.danger,
                            Level::Warn => theme.warning,
                            _ => theme.foreground,
                        };
                        // "HH:MM:SS" is the muted timestamp column.
                        let (time, rest) = line.split_at(line.len().min(8));
                        div()
                            .flex()
                            .gap_2()
                            .py(px(2.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(theme.muted_foreground)
                                    .child(time.to_owned()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_color(color)
                                    .child(rest.trim_start().to_owned()),
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            path.map(|path| path.display().to_string())
                                .unwrap_or_else(|| "Log file unavailable".into()),
                        ),
                    )
                    .child("Auto-scroll")
                    .child(kit::toggle(
                        "log-auto-scroll",
                        self.auto_scroll,
                        "Auto-scroll",
                        move |value, _, cx| {
                            let _ = auto_view.update(cx, |this, cx| {
                                this.auto_scroll = value;
                                if value {
                                    this.scroll.scroll_to_bottom();
                                }
                                cx.notify();
                            });
                        },
                    )),
            )
    }
}

fn reveal_file(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(not(target_os = "macos"))]
    if let Some(folder) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
    }
}

/// Open (or focus) the log window of one screen.
pub fn open_logs(
    key: String,
    title: String,
    diagnostics: Arc<Diagnostics>,
    cx: &mut App,
) -> anyhow::Result<()> {
    open_auxiliary(key, size(px(832.), px(560.)), cx, move |window, cx| {
        window.set_window_title(&title);
        let view = cx.new(|_| LogsView {
            title,
            lines: diagnostics.recent(),
            diagnostics,
            level: Level::All,
            filter: String::new(),
            auto_scroll: true,
            scroll: ScrollHandle::new(),
            fields: Fields::default(),
        });
        let weak = view.downgrade();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                if weak.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        view
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_classified_and_filtered() {
        let info = "06:17:44 [stream] Streaming · 1 viewer";
        let warn = "06:24:52 [display] OS changed resolution → 2560×1440, config synced";
        let error = "06:25:10 [net] Video lane failed: reset";
        assert!(level_of(info) == Level::Info);
        assert!(level_of(warn) == Level::Warn);
        assert!(level_of(error) == Level::Error);
        assert!(shown(info, Level::All, ""));
        assert!(!shown(info, Level::Warn, ""));
        assert!(shown(warn, Level::Warn, "RESOLUTION"));
        assert!(!shown(error, Level::Error, "display"));
    }
}
