#![warn(clippy::all)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod mapped_view;

use std::{collections::VecDeque, mem::size_of, thread, time::Duration};

use compio::time::Interval;
use egui::{Color32, IconData, Stroke, Theme};
use egui_plot::{Line, Plot, PlotPoints};
use flume::Receiver;
use futures::{StreamExt, pin_mut};
use mapped_view::MappedView;
use windows::{
    Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
            WaitForSingleObject,
        },
    },
    core::PCWSTR,
};

/// `WaitForSingleObject` timeout value meaning "wait forever".
const INFINITE: u32 = 0xFFFF_FFFF;

/// Timer period: ~3 ms ≈ 333 Hz.
const PERIOD_MS: u64 = 16;

/// Same period in 100-ns intervals; negative value means relative to now.
const PERIOD_100NS: i64 = -30_000;

/// Mirrors the first three fields of `SPageFilePhysics`.
///
/// The struct uses `#pragma pack(4)` and every field is 4 bytes wide, so the
/// layout is a flat byte blob with no padding:
///
/// | offset | type | field      |
/// |--------|------|------------|
/// |  0     | i32  | packet_id  |
/// |  4     | f32  | throttle   |
/// |  8     | f32  | brake      |
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PhysicsPage {
    packet_id: i32,
    /// Throttle input in the range `0.0–1.0`.
    throttle: f32,
    /// Brake input in the range `0.0–1.0`.
    brake: f32,
}

#[derive(Clone, Copy, Debug)]
struct Inputs {
    throttle: f64,
    brake: f64,
}

impl From<PhysicsPage> for Inputs {
    fn from(page: PhysicsPage) -> Self {
        Self {
            throttle: (page.throttle * 100.) as f64,
            brake: (page.brake * 100.) as f64,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// OPTION A — compio built-in interval
//
// Pro:  No extra thread or Win32 timer boilerplate; fully async.
// Con:  Accuracy is limited by the system-wide timer resolution:
//         • Windows default:              ~15.6 ms
//         • After timeBeginPeriod(1):     ~1.0  ms
//       Fine for most UI overlays; not suitable for guaranteed sub-ms accuracy.
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_compio_timer(view: MappedView, sender: flume::Sender<PhysicsPage>) {
    let mut ticker = compio::time::interval(Duration::from_millis(PERIOD_MS));

    loop {
        ticker.tick().await;
        let PhysicsPage {
            throttle,
            brake,
            packet_id,
        } = unsafe { view.read() };
        println!("[compio timer]   throttle: {throttle:.3}  brake: {brake:.3}");
        sender
            .send(PhysicsPage {
                throttle,
                brake,
                packet_id,
            })
            .ok();
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// OPTION B — dedicated thread + Win32 high-resolution waitable timer
//
// Pro:  ~0.5 ms accuracy via CREATE_WAITABLE_TIMER_HIGH_RESOLUTION (Win10 1803+).
//       Timer ticks are independent of the async runtime's scheduling overhead.
// Con:  One extra OS thread; requires Windows 10 version 1803 or newer.
// ═══════════════════════════════════════════════════════════════════════════════

/// Runs on a dedicated OS thread; never touches the compio runtime.
///
/// Steps:
/// 1. Create a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer.
/// 2. Open the shared-memory view and keep it alive for the thread lifetime.
/// 3. Arm the timer: first tick in `PERIOD_MS` ms, repeat every `PERIOD_MS` ms.
/// 4. Loop: `WaitForSingleObject` → `ptr::read` from the view → `try_send`.
///
/// `try_send` is used so that if the async consumer falls behind, the stale
/// sample is silently dropped rather than blocking the timer thread.
fn hires_timer_thread(tx: flume::Sender<PhysicsPage>) {
    unsafe {
        // 1. High-resolution waitable timer (requires Windows 10 1803+).
        let timer = CreateWaitableTimerExW(
            None,           // default security attributes
            PCWSTR::null(), // anonymous – no name needed
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            0x001F_0003u32, // TIMER_ALL_ACCESS
        )
        .expect("CreateWaitableTimerExW failed – requires Windows 10 1803+");

        // 2. Open the mapping and keep it alive for the lifetime of this thread.
        let view = MappedView::open(
            windows::core::w!("Local\\acevo_pmf_physics"),
            size_of::<PhysicsPage>(),
        )
        .expect("cannot open shared memory – is Assetto Corsa Evo running?");

        // 3. Arm the timer: first tick after PERIOD_MS ms, then every PERIOD_MS ms.
        SetWaitableTimer(
            timer,
            &PERIOD_100NS,    // due time (100-ns units, negative = relative)
            PERIOD_MS as i32, // lPeriod: repeat interval in milliseconds
            None,             // no APC completion routine
            None,             // APC argument (ignored when routine is None)
            false,            // do not wake the system from sleep
        )
        .expect("SetWaitableTimer failed");

        // 4. Sampling loop.
        loop {
            // Blocks until the next timer tick (~PERIOD_MS ms).
            if WaitForSingleObject(timer, INFINITE) != WAIT_OBJECT_0 {
                break; // WAIT_FAILED or WAIT_ABANDONED – exit gracefully.
            }

            // Copy the telemetry snapshot out of the shared-memory mapping.
            let page: PhysicsPage = view.read();

            // Non-blocking send: if the channel already holds an unread sample,
            // this one is discarded so the timer cadence is never compromised.
            let _ = tx.try_send(page);
        }

        let _ = CloseHandle(timer);
    }
}

async fn run_hires_timer() {
    // Capacity-1 channel: at most one pending frame in flight at any time.
    // Older frames are overwritten by try_send in the timer thread.
    let (tx, rx) = flume::bounded::<PhysicsPage>(1);

    thread::spawn(move || hires_timer_thread(tx));

    // recv_async() is runtime-agnostic – compio's IOCP executor polls it fine.
    while let Ok(PhysicsPage {
        throttle, brake, ..
    }) = rx.recv_async().await
    {
        println!("[hi-res thread]  throttle: {throttle:.3}  brake: {brake:.3}");
    }
}

struct State {
    view: MappedView,
    ticker: Interval,
}

// ─── entry point ─────────────────────────────────────────────────────────────
fn main() {
    // ── Uncomment the approach you want to benchmark, comment out the other ──

    let view = MappedView::open(
        windows::core::w!("Local\\acevo_pmf_physics"),
        size_of::<PhysicsPage>(),
    )
    .expect("cannot open shared memory – is Assetto Corsa Evo running?");

    let (sender, receiver) = flume::bounded::<Inputs>(1000);
    let _thread_guard = thread::spawn(move || {
        let _runtime = compio::runtime::Runtime::new().unwrap().block_on(async {
            let ticker = compio::time::interval(Duration::from_millis(PERIOD_MS));
            let data_stream = futures::stream::unfold(State { view, ticker }, async |mut state| {
                state.ticker.tick().await;
                let input = unsafe { state.view.read() };
                return Some((input, state));
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
            // .with_inner_size([win_w, win_h])
            // .with_position(egui::pos2(pos_x, pos_y))
            .with_decorations(false)
            // .with_transparent(true)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native(
        "Pedal Overlay",
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc, receiver)))),
    )
    .unwrap();
    // run_hires_timer().await;
}

pub struct App {
    receiver: Receiver<Inputs>,
    queue: VecDeque<Inputs>,
    drag_pending: bool,
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
            receiver: receiver,
            queue: VecDeque::from(
                [Inputs {
                    throttle: 0.0,
                    brake: 100.0,
                }; 1000],
            ),
            drag_pending: false,
        }
    }
}

impl App {}

impl eframe::App for App {
    /// Called each time the UI needs repainting, which may be many times per second.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui
        let inputs = self.receiver.recv().unwrap();

        self.queue.push_back(inputs);
        if self.queue.len() >= 500 {
            self.queue.pop_front();
        }

        let throttle_line = Line::new(
            "inputs",
            PlotPoints::from_iter(
                self.queue
                    .iter()
                    .enumerate()
                    .map(|(index, input)| [index as f64, input.throttle]),
            ),
        )
        .width(1.0)
        .color(egui::Color32::GREEN);

        let brake_line = Line::new(
            "inputs",
            PlotPoints::from_iter(
                self.queue
                    .iter()
                    .enumerate()
                    .map(|(index, input)| [index as f64, input.brake]),
            ),
        )
        .width(1.0)
        .color(egui::Color32::RED);

        {
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

            if primary_pressed {
                if let Some(pos) = hover_pos {
                    if !grip_rect.contains(pos) {
                        self.drag_pending = true;
                    }
                }
            }

            if !primary_down {
                self.drag_pending = false;
            }

            if self.drag_pending && delta.length_sq() > 0.0 {
                self.drag_pending = false;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            // The central panel the region left after adding TopPanel's and SidePanel's

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
                    plot_ui.line(throttle_line);
                    plot_ui.line(brake_line);
                });
            let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        });

        draw_resize_grip(ui);

        ui.request_repaint();
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
