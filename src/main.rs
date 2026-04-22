#![warn(clippy::all)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod mapped_view;

use std::{collections::VecDeque, mem::size_of, thread, time::Duration};

use compio::time::Interval;
use egui::IconData;
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

struct Inputs {
    throttle: f64,
    brake: f64,
}

impl From<PhysicsPage> for Inputs {
    fn from(page: PhysicsPage) -> Self {
        Self {
            throttle: page.throttle as f64,
            brake: page.brake as f64,
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
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([300.0, 220.0])
            .with_icon(
                IconData::default(), // NOTE: Adding an icon is optional
                                     // eframe::icon_data::from_png_bytes(
                                     //     &include_bytes!("../assets/favicon-512x512.png")[..],
                                     // )
                                     // .expect("Failed to load icon"),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "eframe template",
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc, receiver)))),
    )
    .unwrap();
    // run_hires_timer().await;
}

pub struct App {
    // Example stuff:
    label: String,

    value: f32,
    receiver: Receiver<Inputs>,
    queue: VecDeque<Inputs>,
}

impl App {
    /// Called once before the first frame.
    fn new(cc: &eframe::CreationContext<'_>, receiver: Receiver<Inputs>) -> Self {
        Self {
            label: "Hello World!".to_owned(),
            value: 2.7,
            receiver: receiver,
            queue: VecDeque::with_capacity(1000),
        }
    }
}

impl App {}

impl eframe::App for App {
    /// Called each time the UI needs repainting, which may be many times per second.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui

        // let sin: PlotPoints = (0..1000)
        //     .map(|i| {
        //         let x = i as f64 * 0.01;
        //         [x, x.sin()]
        //     })
        //     .collect();

        // let mut queue = VecDeque::with_capacity(1000);
        let inputs = self.receiver.recv().unwrap();
        // println!(
        //     "[compio timer]  {} throttle: {:.3}  brake: {:.3}",
        //     inputs.packet_id, inputs.throttle, inputs.brake
        // );

        self.queue.push_back(inputs);
        if self.queue.len() >= 1000 {
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
        // egui::Panel::top("top_panel").show_inside(ui, |ui| {
        //     // The top panel is often a good place for a menu bar:

        //     egui::MenuBar::new().ui(ui, |ui| {
        //         // NOTE: no File->Quit on web pages!
        //         ui.menu_button("File", |ui| {
        //             if ui.button("Quit").clicked() {
        //                 ui.send_viewport_cmd(egui::ViewportCommand::Close);
        //             }
        //         });
        //         ui.add_space(16.0);

        //         egui::widgets::global_theme_preference_buttons(ui);
        //     });
        // });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            // The central panel the region left after adding TopPanel's and SidePanel's
            ui.heading("eframe template");

            Plot::new("pedal_inputs")
                // .view_aspect(2.0)
                .view_aspect(6.0) // Keep the plot square since data is 0.0 to 1.0
                .include_x(0.0)
                .include_x(1.0)
                .include_y(0.0)
                .include_y(1.0)
                .show(ui, |plot_ui| {
                    plot_ui.line(throttle_line);
                    plot_ui.line(brake_line);
                });
            // plot_ui.line(Line::new(points).width(2.0).color(egui::Color32::RED));

            // ui.horizontal(|ui| {
            //     ui.label("Write something: ");
            //     ui.text_edit_singleline(&mut self.label);
            // });

            // ui.add(egui::Slider::new(&mut self.value, 0.0..=10.0).text("value"));
            // if ui.button("Increment").clicked() {
            //     self.value += 1.0;
            // }

            // ui.separator();

            // ui.add(egui::github_link_file!(
            //     "https://github.com/emilk/eframe_template/blob/main/",
            //     "Source code."
            // ));

            // ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            //     powered_by_egui_and_eframe(ui);
            //     egui::warn_if_debug_build(ui);
            // });
        });

        ui.request_repaint();
    }
}

// fn powered_by_egui_and_eframe(ui: &mut egui::Ui) {
//     ui.horizontal(|ui| {
//         ui.spacing_mut().item_spacing.x = 0.0;
//         ui.label("Powered by ");
//         ui.hyperlink_to("egui", "https://github.com/emilk/egui");
//         ui.label(" and ");
//         ui.hyperlink_to(
//             "eframe",
//             "https://github.com/emilk/egui/tree/master/crates/eframe",
//         );
//         ui.label(".");
//     });
// }
