//! Desktop UI (egui/eframe). All process data and killing comes from the
//! parent module; this file is only layout, charts and user interaction.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Bar, BarChart, Line, Plot, PlotPoints};
use sysinfo::System;

use crate::{Category, ProcInfo, filter, group_by_unit, human_bytes, kill, snapshot};

const REFRESH: Duration = Duration::from_secs(2);
const HISTORY: usize = 150; // 5 minutes at 2 s per sample
const LIST_LEN: usize = 25;
const MIB: f64 = 1024.0 * 1024.0;

const APP_COLOR: Color32 = Color32::from_rgb(90, 160, 255);
const BG_COLOR: Color32 = Color32::from_rgb(255, 165, 70);
const OTHER_COLOR: Color32 = Color32::from_rgb(150, 150, 160);
const FREE_COLOR: Color32 = Color32::from_rgb(90, 200, 120);
const HOT_COLOR: Color32 = Color32::from_rgb(240, 90, 90);

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Overview,
    Highest,
    Lowest,
    Apps,
    Background,
    All,
}

struct Confirm {
    pid: u32,
    name: String,
    force: bool,
}

pub struct RamApp {
    sys: System,
    procs: Vec<ProcInfo>,
    /// (seconds since start, used RAM in GiB)
    history: VecDeque<[f64; 2]>,
    started: Instant,
    last_refresh: Instant,
    view: View,
    query: String,
    confirm: Option<Confirm>,
    status: String,
}

impl Default for RamApp {
    fn default() -> Self {
        let mut app = RamApp {
            sys: System::new(),
            procs: Vec::new(),
            history: VecDeque::new(),
            started: Instant::now(),
            last_refresh: Instant::now(),
            view: View::Overview,
            query: String::new(),
            confirm: None,
            status: String::new(),
        };
        app.refresh();
        app
    }
}

fn cat_color(c: Category) -> Color32 {
    match c {
        Category::App => APP_COLOR,
        Category::Background => BG_COLOR,
        Category::Kernel => OTHER_COLOR,
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// Horizontal bar chart of (label, bytes, color), biggest at the top.
fn bar_chart(ui: &mut Ui, id: &str, items: Vec<(String, u64, Color32)>, height: f32) {
    let n = items.len();
    let labels: Vec<String> = items.iter().map(|(l, _, _)| short(l, 22)).collect();
    let bars = items
        .iter()
        .enumerate()
        .map(|(i, (label, bytes, color))| {
            Bar::new((n - 1 - i) as f64, *bytes as f64 / MIB)
                .name(format!("{label}\n{}", human_bytes(*bytes)))
                .fill(*color)
                .width(0.7)
        })
        .collect();
    Plot::new(id)
        .height(height)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show_grid([true, false])
        .include_x(0.0)
        .x_axis_formatter(|m, _| human_bytes((m.value.max(0.0) * MIB) as u64))
        .y_axis_formatter(move |m, _| {
            let i = m.value.round();
            if (m.value - i).abs() > 0.01 || i < 0.0 || i as usize >= n {
                return String::new();
            }
            labels[n - 1 - i as usize].clone()
        })
        .label_formatter(|_| None)
        .show(ui, |p| {
            p.bar_chart(
                BarChart::new(id, bars)
                    .horizontal()
                    .element_formatter(Box::new(|b, _| b.name.clone())),
            )
        });
}

fn stat_card(ui: &mut Ui, title: &str, value: String, sub: String, color: Color32) {
    egui::Frame::group(ui.style())
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width(205.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).small());
                ui.label(RichText::new(value).size(32.0).strong().color(color));
                ui.label(RichText::new(sub).small().weak());
            });
        });
}

/// Large, easy-to-read text and controls.
pub fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(30.0, Proportional)),
            (TextStyle::Body, FontId::new(19.0, Proportional)),
            (TextStyle::Button, FontId::new(19.0, Proportional)),
            (TextStyle::Small, FontId::new(16.0, Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(18.0, egui::FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.interact_size.y = 32.0;
    });
}

impl RamApp {
    pub fn refresh(&mut self) {
        self.procs = snapshot(&mut self.sys);
        let used = self.sys.used_memory() as f64 / (MIB * 1024.0);
        self.history
            .push_back([self.started.elapsed().as_secs_f64(), used]);
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.last_refresh = Instant::now();
    }

    fn sum(&self, cat: Category) -> (u64, usize) {
        self.procs
            .iter()
            .filter(|p| p.category == cat)
            .fold((0, 0), |(m, n), p| (m + p.mem, n + 1))
    }

    fn rows(&self) -> Vec<ProcInfo> {
        let base: Vec<&ProcInfo> = match self.view {
            View::Overview => return Vec::new(),
            View::Highest => self.procs.iter().take(LIST_LEN).collect(),
            View::Lowest => self
                .procs
                .iter()
                .rev()
                .filter(|p| p.mem > 0)
                .take(LIST_LEN)
                .collect(),
            View::Apps => self
                .procs
                .iter()
                .filter(|p| p.category == Category::App)
                .collect(),
            View::Background => self
                .procs
                .iter()
                .filter(|p| p.category == Category::Background)
                .collect(),
            View::All => self.procs.iter().collect(),
        };
        let owned: Vec<ProcInfo> = base.into_iter().cloned().collect();
        filter(&owned, &self.query).into_iter().cloned().collect()
    }

    fn nav(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        ui.heading("ramtop");
        ui.add_space(8.0);
        let (app_mem, app_n) = self.sum(Category::App);
        let (bg_mem, bg_n) = self.sum(Category::Background);
        let items = [
            (View::Overview, "📊 Overview".to_string()),
            (View::Highest, "🔥 Highest consumers".to_string()),
            (View::Lowest, "🍃 Lowest consumers".to_string()),
            (
                View::Apps,
                format!("🖥 Apps ({app_n}, {})", human_bytes(app_mem)),
            ),
            (
                View::Background,
                format!("⚙ Background ({bg_n}, {})", human_bytes(bg_mem)),
            ),
            (
                View::All,
                format!("📋 All processes ({})", self.procs.len()),
            ),
        ];
        for (view, label) in items {
            if ui.selectable_label(self.view == view, label).clicked() && self.view != view {
                self.view = view;
                self.query.clear();
            }
        }
        ui.add_space(12.0);
        if ui.button("⟳ Refresh now").clicked() {
            self.refresh();
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        let (used, total) = (self.sys.used_memory(), self.sys.total_memory());
        let (su, st) = (self.sys.used_swap(), self.sys.total_swap());
        let frac = |a: u64, b: u64| if b == 0 { 0.0 } else { a as f32 / b as f32 };
        ui.horizontal(|ui| {
            for (name, u, t) in [("RAM", used, total), ("Swap", su, st)] {
                let f = frac(u, t);
                ui.label(RichText::new(name).strong());
                ui.add(
                    egui::ProgressBar::new(f)
                        .desired_width(260.0)
                        .desired_height(22.0)
                        .fill(if f > 0.85 { HOT_COLOR } else { FREE_COLOR }),
                );
                ui.label(format!(
                    "{} / {}  ({:.0}%)",
                    human_bytes(u),
                    human_bytes(t),
                    f * 100.0
                ));
                ui.add_space(16.0);
            }
        });
        if !self.status.is_empty() {
            ui.label(RichText::new(&self.status).color(Color32::YELLOW));
        }
    }

    fn overview(&mut self, ui: &mut Ui) {
        let total = self.sys.total_memory();
        let used = self.sys.used_memory();
        let (app_mem, app_n) = self.sum(Category::App);
        let (bg_mem, bg_n) = self.sum(Category::Background);
        let top = self.procs.first();

        ui.horizontal_wrapped(|ui| {
            stat_card(
                ui,
                "RAM used",
                human_bytes(used),
                format!("of {}", human_bytes(total)),
                if used as f64 > total as f64 * 0.85 {
                    HOT_COLOR
                } else {
                    FREE_COLOR
                },
            );
            stat_card(
                ui,
                "Swap used",
                human_bytes(self.sys.used_swap()),
                format!("of {}", human_bytes(self.sys.total_swap())),
                OTHER_COLOR,
            );
            stat_card(
                ui,
                "Apps",
                human_bytes(app_mem),
                format!("{app_n} processes"),
                APP_COLOR,
            );
            stat_card(
                ui,
                "Background",
                human_bytes(bg_mem),
                format!("{bg_n} processes"),
                BG_COLOR,
            );
            if let Some(p) = top {
                stat_card(
                    ui,
                    "Biggest process",
                    human_bytes(p.mem),
                    format!("{} (PID {})", short(&p.name, 20), p.pid),
                    HOT_COLOR,
                );
            }
        });
        ui.add_space(10.0);

        ui.strong("RAM usage over time");
        let hist: Vec<[f64; 2]> = self.history.iter().copied().collect();
        let total_gib = total as f64 / (MIB * 1024.0);
        Plot::new("history")
            .height(240.0)
            .include_y(0.0)
            .include_y(total_gib)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .y_axis_formatter(|m, _| format!("{:.0} GiB", m.value))
            .x_axis_formatter(|m, _| format!("{:.0}s", m.value))
            .show(ui, |p| {
                p.line(
                    Line::new("Used RAM", PlotPoints::from(hist))
                        .color(HOT_COLOR)
                        .fill(0.0)
                        .fill_alpha(0.15),
                )
            });
        ui.add_space(10.0);

        ui.columns(2, |cols| {
            cols[0].strong("Top 10 processes");
            let items = self
                .procs
                .iter()
                .take(10)
                .map(|p| (p.name.clone(), p.mem, cat_color(p.category)))
                .collect();
            bar_chart(&mut cols[0], "top10", items, 400.0);

            cols[1].strong("Where the RAM goes");
            let rss = app_mem + bg_mem;
            let mut items = vec![
                ("Apps".to_string(), app_mem, APP_COLOR),
                ("Background".to_string(), bg_mem, BG_COLOR),
                (
                    "Other (kernel, cache)".to_string(),
                    used.saturating_sub(rss),
                    OTHER_COLOR,
                ),
                ("Free".to_string(), total.saturating_sub(used), FREE_COLOR),
            ];
            // Shared pages are counted in every process' RSS, so "other" can be 0.
            items.retain(|(_, bytes, _)| *bytes > 0);
            bar_chart(&mut cols[1], "breakdown", items, 400.0);
        });
        ui.horizontal(|ui| {
            for (c, l) in [(APP_COLOR, "App"), (BG_COLOR, "Background")] {
                ui.label(RichText::new("■").color(c));
                ui.label(l);
            }
        });
    }

    fn list_view(&mut self, ui: &mut Ui) {
        let rows = self.rows();
        let (title, chart): (&str, Vec<(String, u64, Color32)>) = match self.view {
            View::Highest => (
                "Highest RAM consumers",
                rows.iter()
                    .take(15)
                    .map(|p| (p.name.clone(), p.mem, cat_color(p.category)))
                    .collect(),
            ),
            View::Lowest => (
                "Lowest RAM consumers (excluding kernel threads)",
                rows.iter()
                    .take(15)
                    .map(|p| (p.name.clone(), p.mem, cat_color(p.category)))
                    .collect(),
            ),
            View::Apps => (
                "Apps — RAM per app",
                group_by_unit(&rows)
                    .into_iter()
                    .take(12)
                    .map(|(u, m, n)| (format!("{u} ×{n}"), m, APP_COLOR))
                    .collect(),
            ),
            View::Background => (
                "Background services — RAM per service",
                group_by_unit(&rows)
                    .into_iter()
                    .take(12)
                    .map(|(u, m, n)| (format!("{u} ×{n}"), m, BG_COLOR))
                    .collect(),
            ),
            _ => ("All processes", Vec::new()),
        };
        ui.heading(title);
        if !chart.is_empty() {
            let h = 34.0 * chart.len() as f32 + 50.0;
            bar_chart(ui, title, chart, h.min(420.0));
        }
        ui.horizontal(|ui| {
            ui.label("🔍 Filter:");
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("name, service, command or PID")
                    .desired_width(420.0)
                    .margin(egui::vec2(10.0, 8.0)),
            );
            ui.label(format!("{} shown", rows.len()));
        });
        self.table(ui, &rows);
    }

    fn table(&mut self, ui: &mut Ui, rows: &[ProcInfo]) {
        let total = self.sys.total_memory().max(1);
        let mut action = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::exact(90.0))
            .column(Column::initial(220.0).clip(true))
            .column(Column::exact(120.0))
            .column(Column::exact(90.0))
            .column(Column::exact(130.0))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(270.0))
            .header(36.0, |mut h| {
                for t in ["PID", "Name", "RAM", "% RAM", "Type", "Service", "Actions"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(40.0, rows.len(), |mut row| {
                    let p = &rows[row.index()];
                    let pct = p.mem as f64 * 100.0 / total as f64;
                    let hot = pct >= 10.0;
                    row.col(|ui| {
                        ui.label(p.pid.to_string());
                    });
                    row.col(|ui| {
                        ui.label(&p.name).on_hover_text(&p.cmd);
                    });
                    row.col(|ui| {
                        let t = RichText::new(human_bytes(p.mem));
                        ui.label(if hot { t.color(HOT_COLOR).strong() } else { t });
                    });
                    row.col(|ui| {
                        ui.label(format!("{pct:.1}%"));
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(p.category.label()).color(cat_color(p.category)));
                    });
                    row.col(|ui| {
                        ui.label(&p.unit);
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Terminate").clicked() {
                                action = Some((p.pid, p.name.clone(), false));
                            }
                            if ui
                                .button(RichText::new("Force kill").color(HOT_COLOR))
                                .clicked()
                            {
                                action = Some((p.pid, p.name.clone(), true));
                            }
                        });
                    });
                });
            });
        if let Some((pid, name, force)) = action {
            self.confirm = Some(Confirm { pid, name, force });
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = &self.confirm else { return };
        let (verb, detail) = if c.force {
            (
                "Force kill",
                "SIGKILL — stops it immediately, unsaved data is lost.",
            )
        } else {
            ("Terminate", "SIGTERM — asks it to shut down cleanly.")
        };
        let mut answer = None;
        egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading(format!("{verb} {}?", c.name));
            ui.label(format!("PID {}", c.pid));
            ui.label(detail);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new(format!("Yes, {}", verb.to_lowercase())).color(HOT_COLOR))
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                let sig = if c.force { "SIGKILL" } else { "SIGTERM" };
                self.status = match kill(c.pid, c.force) {
                    Ok(()) => format!("Sent {sig} to {} (PID {})", c.name, c.pid),
                    Err(e) => format!("Error: {e}"),
                };
                self.confirm = None;
                self.refresh();
            }
            Some(false) => {
                self.status = "Cancelled".into();
                self.confirm = None;
            }
            None => {}
        }
    }
}

impl eframe::App for RamApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
        ctx.request_repaint_after(REFRESH);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("nav")
            .exact_size(340.0)
            .show(ui, |ui| self.nav(ui));
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Overview => {
                egui::ScrollArea::vertical().show(ui, |ui| self.overview(ui));
            }
            _ => self.list_view(ui),
        });
        let ctx = ui.ctx().clone();
        self.confirm_dialog(&ctx);
    }
}
