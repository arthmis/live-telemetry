// ═══════════════════════════════════════════════════════════════════════════════
// OPTION A — compio built-in interval
//
// Pro:  No extra thread or Win32 timer boilerplate; fully async.
// Con:  Accuracy is limited by the system-wide timer resolution:
//         • Windows default:              ~15.6 ms
//         • After timeBeginPeriod(1):     ~1.0  ms
//       Fine for most UI overlays; not suitable for guaranteed sub-ms accuracy.
// ═══════════════════════════════════════════════════════════════════════════════

// async fn run_compio_timer(view: MappedView, sender: flume::Sender<PhysicsPage>) {
//     let mut ticker = compio::time::interval(Duration::from_millis(PERIOD_MS));

//     loop {
//         ticker.tick().await;
//         let physics_page = unsafe { view.read() };
//         sender.send(physics_page).ok();
//     }
// }

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
// fn hires_timer_thread(tx: flume::Sender<PhysicsPage>) {
//     unsafe {
//         // 1. High-resolution waitable timer (requires Windows 10 1803+).
//         let timer = CreateWaitableTimerExW(
//             None,           // default security attributes
//             PCWSTR::null(), // anonymous – no name needed
//             CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
//             0x001F_0003u32, // TIMER_ALL_ACCESS
//         )
//         .expect("CreateWaitableTimerExW failed – requires Windows 10 1803+");

//         // 2. Open the mapping and keep it alive for the lifetime of this thread.
//         let view = MappedView::open(
//             windows::core::w!("Local\\acevo_pmf_physics"),
//             size_of::<PhysicsPage>(),
//         )
//         .expect("cannot open shared memory – is Assetto Corsa Evo running?");

//         // 3. Arm the timer: first tick after PERIOD_MS ms, then every PERIOD_MS ms.
//         SetWaitableTimer(
//             timer,
//             &PERIOD_100NS,    // due time (100-ns units, negative = relative)
//             PERIOD_MS as i32, // lPeriod: repeat interval in milliseconds
//             None,             // no APC completion routine
//             None,             // APC argument (ignored when routine is None)
//             false,            // do not wake the system from sleep
//         )
//         .expect("SetWaitableTimer failed");

//         // 4. Sampling loop.
//         loop {
//             // Blocks until the next timer tick (~PERIOD_MS ms).
//             if WaitForSingleObject(timer, INFINITE) != WAIT_OBJECT_0 {
//                 break; // WAIT_FAILED or WAIT_ABANDONED – exit gracefully.
//             }

//             // Copy the telemetry snapshot out of the shared-memory mapping.
//             let page: PhysicsPage = view.read();

//             // Non-blocking send: if the channel already holds an unread sample,
//             // this one is discarded so the timer cadence is never compromised.
//             let _ = tx.try_send(page);
//         }

//         let _ = CloseHandle(timer);
//     }
// }

// async fn run_hires_timer() {
//     // Capacity-1 channel: at most one pending frame in flight at any time.
//     // Older frames are overwritten by try_send in the timer thread.
//     let (tx, rx) = flume::bounded::<PhysicsPage>(1);

//     thread::spawn(move || hires_timer_thread(tx));

//     // recv_async() is runtime-agnostic – compio's IOCP executor polls it fine.
//     while let Ok(PhysicsPage {
//         throttle, brake, ..
//     }) = rx.recv_async().await
//     {
//         println!("[hi-res thread]  throttle: {throttle:.3}  brake: {brake:.3}");
//     }
// }
