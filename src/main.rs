#![warn(clippy::all)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::{collections::VecDeque, mem::size_of, thread, time::Duration};

use compio::time::Interval;
use egui::{Color32, IconData, Stroke, Theme};
use egui_plot::{Line, Plot, PlotPoints};
use flume::Receiver;
use futures::{StreamExt, pin_mut};
use telemetry_overlay::{Inputs, PERIOD_MS, PhysicsPage, mapped_view::MappedView};

struct State {
    view: MappedView,
    ticker: Interval,
}

fn main() {
    let (sender, receiver) = flume::bounded::<Inputs>(1000);
    let _thread_guard = thread::spawn(move || {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let view: MappedView = loop {
                let view = MappedView::open(
                    windows::core::w!("Local\\acevo_pmf_physics"),
                    size_of::<PhysicsPage>(),
                );

                match view {
                    Err(_) => {
                        thread::sleep(Duration::from_millis(5000));
                        continue;
                    }
                    Ok(view) => break view,
                }
            };

            let ticker = compio::time::interval(Duration::from_millis(PERIOD_MS));
            let data_stream = futures::stream::unfold(State { view, ticker }, async |mut state| {
                state.ticker.tick().await;
                let input = unsafe { state.view.read() };
                Some((input, state))
            });

            pin_mut!(data_stream);
            while let Some(physics) = data_stream.next().await {
                sender.send(Inputs::from(physics)).ok();
            }
        });
    });

    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 125.0])
            .with_min_inner_size([300.0, 100.0])
            .with_icon(
                IconData::default(), // NOTE: Adding an icon is optional
                                     // eframe::icon_data::from_png_bytes(
                                     //     &include_bytes!("../assets/favicon-512x512.png")[..],
                                     // )
                                     // .expect("Failed to load icon"),
            )
            .with_title("Pedal Overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native(
        "Pedal Overlay",
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc, receiver)))),
    )
    .unwrap();
}

pub struct App {
    receiver: Receiver<Inputs>,
    queue: VecDeque<Inputs>,
    drag_pending: bool,
    is_receiving_data: bool,
}

impl App {
    /// Called once before the first frame.
    fn new(cc: &eframe::CreationContext<'_>, receiver: Receiver<Inputs>) -> Self {
        let visuals = egui::Visuals {
            dark_mode: true,
            panel_fill: Color32::TRANSPARENT,
            window_fill: Color32::TRANSPARENT,
            override_text_color: Some(Color32::from_rgb(220, 225, 232)),
            ..egui::Visuals::dark()
        };
        cc.egui_ctx.set_visuals_of(Theme::Dark, visuals);
        cc.egui_ctx.set_theme(Theme::Dark);

        Self {
            receiver,
            queue: VecDeque::from([Inputs::new(0.0, 100.0); 1000]),
            drag_pending: false,
            is_receiving_data: false,
        }
    }
}

impl App {}

impl eframe::App for App {
    /// Called each time the UI needs repainting, which may be many times per second.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui

        if let Ok(inputs) = self.receiver.try_recv() {
            self.queue.push_back(inputs);
            self.queue.pop_front();
            self.is_receiving_data = true;
        };

        if !self.is_receiving_data {
            draw_game_not_running_fallback(ui);
            draw_close_button(ui);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(10000));
            return;
        }

        add_window_drag(ui, self);

        egui::CentralPanel::default().show_inside(ui, |ui| {
            // The central panel the region left after adding TopPanel's and SidePanel's
            draw_input_lines(ui, &self.queue);
            let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        });

        draw_resize_grip(ui);
        draw_close_button(ui);

        // necessary or egui won't repaint since it goes idle if there aren't any user interaction events
        ui.request_repaint();
    }
}

pub fn draw_game_not_running_fallback(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show_inside(ui, |ui| {
        ui.centered_and_justified(|ui| ui.heading("Start driving"));

        let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        draw_resize_grip(ui);
    });
}

pub fn draw_input_lines(ui: &mut egui::Ui, queue: &VecDeque<Inputs>) {
    let input_lines = line_from_inputs(queue);
    Plot::new("pedal_inputs")
        .include_x(0.0)
        .include_y(0.0)
        .include_y(100.0)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .allow_scroll(false)
        .allow_double_click_reset(false)
        .show_axes([false, true])
        .show_grid([false, true])
        .show_background(false)
        .show_x(false)
        .show_y(false)
        .clamp_grid(true)
        .show(ui, |plot_ui| {
            plot_ui.line(input_lines.throttle);
            plot_ui.line(input_lines.brake);
        });
}

pub fn add_window_drag(ui: &mut egui::Ui, state: &mut App) {
    let panel_rect = ui.max_rect();
    let grip_size = 14.0;
    let grip_rect = egui::Rect::from_min_size(
        egui::pos2(
            panel_rect.right() - grip_size,
            panel_rect.bottom() - grip_size,
        ),
        egui::Vec2::splat(grip_size),
    );

    let (primary_pressed, primary_down, delta, hover_pos) = ui.ctx().input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.delta(),
            i.pointer.hover_pos(),
        )
    });

    let close_size = 20.0;
    let close_rect = egui::Rect::from_min_size(
        egui::pos2(
            panel_rect.right() - close_size - 4.0,
            panel_rect.top() + 4.0,
        ),
        egui::Vec2::splat(close_size),
    );

    if primary_pressed
        && let Some(pos) = hover_pos
        && !grip_rect.contains(pos)
        && !close_rect.contains(pos)
    {
        state.drag_pending = true;
    }

    if !primary_down {
        state.drag_pending = false;
    }

    if state.drag_pending && delta.length_sq() > 0.0 {
        state.drag_pending = false;
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

pub fn draw_resize_grip(ui: &mut egui::Ui) {
    let panel_rect = ui.max_rect();
    let grip_size = 14.0;
    let grip_rect = egui::Rect::from_min_size(
        egui::pos2(
            panel_rect.right() - grip_size,
            panel_rect.bottom() - grip_size,
        ),
        egui::Vec2::splat(grip_size),
    );

    let resp = ui.interact(
        grip_rect,
        ui.id().with("resize_grip"),
        egui::Sense::click_and_drag(),
    );

    let p = ui.painter();
    let br = grip_rect.right_bottom();
    let col = Color32::from_rgba_unmultiplied(180, 180, 200, 160);
    p.line_segment(
        [
            br - egui::vec2(grip_size, 0.0),
            br - egui::vec2(0.0, grip_size),
        ],
        Stroke::new(1.5, col),
    );
    p.line_segment(
        [
            br - egui::vec2(grip_size * 0.55, 0.0),
            br - egui::vec2(0.0, grip_size * 0.55),
        ],
        Stroke::new(1.5, col),
    );
    if resp.drag_started() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                egui::ResizeDirection::SouthEast,
            ));
    }

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
        ui.ctx().request_repaint();
    }
}

pub fn draw_close_button(ui: &mut egui::Ui) {
    let panel_rect = ui.max_rect();
    let btn_size = 20.0;
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(panel_rect.right() - btn_size - 4.0, panel_rect.top() + 4.0),
        egui::Vec2::splat(btn_size),
    );

    let resp = ui.interact(btn_rect, ui.id().with("close_button"), egui::Sense::click());

    let p = ui.painter();

    let bg = if resp.hovered() {
        Color32::from_rgba_unmultiplied(210, 50, 50, 220)
    } else {
        Color32::from_rgba_unmultiplied(80, 80, 100, 140)
    };
    p.rect_filled(btn_rect, 4.0, bg);

    let margin = 5.0;
    let x_col = Color32::from_rgb(230, 230, 235);
    p.line_segment(
        [
            btn_rect.min + egui::vec2(margin, margin),
            btn_rect.max - egui::vec2(margin, margin),
        ],
        Stroke::new(1.5, x_col),
    );
    p.line_segment(
        [
            egui::pos2(btn_rect.max.x - margin, btn_rect.min.y + margin),
            egui::pos2(btn_rect.min.x + margin, btn_rect.max.y - margin),
        ],
        Stroke::new(1.5, x_col),
    );

    if resp.clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.ctx().request_repaint();
    }
}

fn line_from_inputs(queue: &VecDeque<Inputs>) -> InputLines<'_> {
    let line_width = 2.0;
    let brake_color = egui::Color32::RED;
    let throttle_color = egui::Color32::GREEN;
    let throttle_line = Line::new(
        "inputs",
        PlotPoints::from_iter(
            queue
                .iter()
                .enumerate()
                .map(|(index, input)| [index as f64, input.throttle()]),
        ),
    )
    .width(line_width)
    .color(throttle_color);

    let brake_line = Line::new(
        "inputs",
        PlotPoints::from_iter(
            queue
                .iter()
                .enumerate()
                .map(|(index, input)| [index as f64, input.brake()]),
        ),
    )
    .width(line_width)
    .color(brake_color);

    InputLines {
        throttle: throttle_line,
        brake: brake_line,
    }
}

struct InputLines<'a> {
    pub throttle: Line<'a>,
    pub brake: Line<'a>,
}
