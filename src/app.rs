//! The eframe application: tabs, each holding one or more split panes. The
//! active tab's panes are laid out, driven, and painted in a single GPU callback.

use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use eframe::egui_wgpu;

use crate::config::Config;
use crate::render::{self, PaneFrame, TermFrame};
use crate::session::Session;

/// A tab: one or more panes split along a single axis, with one focused.
struct Tab {
    panes: Vec<Session>,
    focus: usize,
    /// true = side-by-side columns, false = stacked rows.
    vertical: bool,
}

impl Tab {
    fn single(session: Session) -> Self {
        Self {
            panes: vec![session],
            focus: 0,
            vertical: true,
        }
    }
    fn focused(&self) -> &Session {
        &self.panes[self.focus]
    }
}

pub struct App {
    tabs: Vec<Tab>,
    active_tab: usize,
    /// Cell size in physical pixels (from the glyph atlas), shared by all panes.
    cell_w: f32,
    cell_h: f32,
    config: Config,
    egui_ctx: egui::Context,
    last_window_title: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .expect("eframe must run with the wgpu backend");

        let config = Config::load();
        let ppp = cc.egui_ctx.pixels_per_point().max(1.0);
        let px = (config.font_points * ppp).round();
        let (cell_w, cell_h) = render::init(render_state, px);

        let first = Session::new(&cc.egui_ctx, &config)?;

        Ok(Self {
            tabs: vec![Tab::single(first)],
            active_tab: 0,
            cell_w,
            cell_h,
            config,
            egui_ctx: cc.egui_ctx.clone(),
            last_window_title: None,
        })
    }

    fn spawn_session(&self) -> Option<Session> {
        Session::new(&self.egui_ctx, &self.config)
            .map_err(|e| eprintln!("giest: failed to open session: {e}"))
            .ok()
    }

    fn new_tab(&mut self) {
        if let Some(s) = self.spawn_session() {
            self.tabs.push(Tab::single(s));
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// Split the focused pane along `vertical` axis, focusing the new pane.
    fn split(&mut self, vertical: bool) {
        if let Some(s) = self.spawn_session() {
            let tab = &mut self.tabs[self.active_tab];
            tab.vertical = vertical;
            tab.panes.push(s);
            tab.focus = tab.panes.len() - 1;
        }
    }

    /// Close the focused pane; closing the last pane closes the tab, and the
    /// last tab closes the window.
    fn close_focused(&mut self, ctx: &egui::Context) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.panes.len() > 1 {
            tab.panes.remove(tab.focus);
            tab.focus = tab.focus.min(tab.panes.len() - 1);
        } else if self.tabs.len() > 1 {
            self.tabs.remove(self.active_tab);
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// App-level shortcuts (reserved by `Session::handle_input`, never sent to
    /// the shell): tabs, splits, focus switching.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in &events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if modifiers.ctrl && modifiers.shift {
                match key {
                    egui::Key::T => self.new_tab(),
                    egui::Key::W => self.close_focused(ctx),
                    egui::Key::D => self.split(true),  // vertical (columns)
                    egui::Key::E => self.split(false), // horizontal (rows)
                    _ => {}
                }
            } else if modifiers.ctrl && *key == egui::Key::Tab {
                let n = self.tabs.len();
                if n > 1 {
                    self.active_tab = if modifiers.shift {
                        (self.active_tab + n - 1) % n
                    } else {
                        (self.active_tab + 1) % n
                    };
                }
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut switch_to = None;
        let mut want_new = false;
        ui.horizontal(|ui| {
            for (i, tab) in self.tabs.iter().enumerate() {
                let raw = tab
                    .focused()
                    .title()
                    .unwrap_or_else(|| format!("shell {}", i + 1));
                let mut label = ellipsize(&raw, 24);
                if tab.panes.len() > 1 {
                    label = format!("{label} [{}]", tab.panes.len());
                }
                if ui.selectable_label(i == self.active_tab, label).clicked() {
                    switch_to = Some(i);
                }
            }
            if ui.button("+").on_hover_text("New tab (Ctrl+Shift+T)").clicked() {
                want_new = true;
            }
        });
        if let Some(i) = switch_to {
            self.active_tab = i;
        }
        if want_new {
            self.new_tab();
        }
    }

    /// Lay out, drive, and paint the active tab's panes.
    fn render_active(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (cw, ch) = (self.cell_w, self.cell_h);
        let ppp = ctx.pixels_per_point().max(1.0);
        let area = ui.max_rect();
        let active_tab = self.active_tab;
        let tab = &mut self.tabs[active_tab];
        let rects = pane_rects(tab.panes.len(), tab.vertical, area);

        // Focus-follows-click: a press inside a pane focuses it.
        let press_pos = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos, pressed: true, ..
                } => Some(*pos),
                _ => None,
            })
        });
        if let Some(pos) = press_pos {
            if let Some(i) = rects.iter().position(|r| r.contains(pos)) {
                tab.focus = i;
            }
        }
        let focus = tab.focus.min(tab.panes.len() - 1);
        tab.focus = focus;

        // Keyboard goes to the focused pane.
        let tracking = tab.panes[focus].is_mouse_tracking();
        tab.panes[focus].handle_input(ctx, tracking, ch);

        let mut frames: Vec<PaneFrame> = Vec::with_capacity(tab.panes.len());
        for (i, &prect) in rects.iter().enumerate() {
            let session = &mut tab.panes[i];
            session.fit_grid(prect, ppp, cw, ch);
            if !session.update_snapshot() {
                continue;
            }

            // Mouse / selection interaction only for the focused pane.
            if i == focus {
                if tracking {
                    session.handle_mouse(ctx, prect, ppp, cw, ch);
                    session.clear_selection();
                } else {
                    let resp = ui.interact(
                        prect,
                        egui::Id::new(("giest-pane", active_tab, i)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.drag_started() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = session.pos_to_cell(p, prect, ppp, cw, ch);
                            session.begin_selection(c);
                        }
                    } else if resp.dragged() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = session.pos_to_cell(p, prect, ppp, cw, ch);
                            session.update_selection(c);
                        }
                    }
                    if resp.clicked() {
                        session.clear_selection();
                    }
                }
            }

            let mut snapshot = session.snapshot.clone();
            if i != focus {
                snapshot.cursor_visible = false; // only the focused pane shows a cursor
            } else if snapshot.cursor_blinking {
                if ctx.input(|i| i.time) % 1.0 >= 0.5 {
                    snapshot.cursor_visible = false;
                }
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            frames.push(PaneFrame {
                snapshot,
                origin_px: [prect.min.x * ppp, prect.min.y * ppp],
                selection: session.selection_range(),
            });
        }

        // Fill the whole area with the focused pane's background first.
        let bg = tab.panes[focus].default_bg();
        ui.painter()
            .rect_filled(area, 0.0, egui::Color32::from_rgb(bg.r, bg.g, bg.b));

        // One callback paints every pane (shared instance buffer).
        ui.painter()
            .add(egui_wgpu::Callback::new_paint_callback(area, TermFrame { panes: frames }));

        // Highlight the focused pane when split.
        if rects.len() > 1 {
            ui.painter().rect_stroke(
                rects[focus],
                0.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(90, 130, 200)),
                egui::StrokeKind::Inside,
            );
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Pump every pane in every tab so background sessions keep flowing.
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.pump_pty();
            }
        }
        self.handle_shortcuts(&ctx);

        // Window title from the active tab's focused pane.
        let title = self.tabs.get(self.active_tab).and_then(|t| t.focused().title());
        if title != self.last_window_title {
            let shown = title.clone().unwrap_or_else(|| "giest".to_string());
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(shown));
            self.last_window_title = title;
        }

        if self.tabs.len() > 1 {
            egui::Panel::top("giest-tabs").show_inside(ui, |ui| self.tab_bar(ui));
        }

        let bg = self.tabs[self.active_tab].focused().default_bg();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(bg.r, bg.g, bg.b)))
            .show_inside(ui, |ui| self.render_active(ui, &ctx));
    }
}

/// Divide `area` into `n` equal panes along the split axis (with a 1px gutter).
fn pane_rects(n: usize, vertical: bool, area: egui::Rect) -> Vec<egui::Rect> {
    let n = n.max(1);
    let gap = 1.0;
    (0..n)
        .map(|i| {
            let f = i as f32;
            if vertical {
                let w = area.width() / n as f32;
                egui::Rect::from_min_size(
                    egui::pos2(area.min.x + f * w, area.min.y),
                    egui::vec2((w - gap).max(1.0), area.height()),
                )
            } else {
                let h = area.height() / n as f32;
                egui::Rect::from_min_size(
                    egui::pos2(area.min.x, area.min.y + f * h),
                    egui::vec2(area.width(), (h - gap).max(1.0)),
                )
            }
        })
        .collect()
}

/// Truncate a tab label to `max` chars with an ellipsis.
fn ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}
