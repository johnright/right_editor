#![cfg_attr(all(windows, feature = "gui-subsystem"), windows_subsystem = "windows")]

mod app;
mod clipboard;
mod config;
mod editor;
mod input;
mod latency;
mod nvim;
mod preview;
mod renderer;
mod vsync;
mod window_state;

use anyhow::Result;
use winit::event_loop::{ControlFlow, EventLoop};

use app::UserEvent;

/// Placeholder for the attach. nvim starts before fonts are loaded; the real
/// size follows via ui_try_resize.
const INITIAL_GRID: (u64, u64) = (100, 30);

fn main() -> Result<()> {
    let config = config::Config::load(std::env::args().skip(1))?;

    env_logger::Builder::new()
        .parse_filters(&config.log_filter)
        .format_timestamp_millis()
        .init();
    log::info!("config: {config:?}");

    // Font scan takes a few hundred ms, run it alongside nvim startup and window creation.
    let fonts = std::thread::Builder::new().name("fonts".into()).spawn(|| {
        let start = std::time::Instant::now();
        let system = cosmic_text::FontSystem::new();
        log::info!("system fonts loaded in {:?}", start.elapsed());
        system
    })?;

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    // no polling, wake only on events
    event_loop.set_control_flow(ControlFlow::Wait);

    // tokio gets its own threads, the window stays on main
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("nvim-rpc")
        .enable_all()
        .build()?;
    // Start nvim now, wsl.exe is the slow part of startup.
    let (cols, rows) = INITIAL_GRID;
    let (nvim_tx, nvim_rx) = tokio::sync::mpsc::unbounded_channel();
    runtime.spawn(nvim::run(
        config.nvim.clone(),
        config.multigrid,
        cols,
        rows,
        event_loop.create_proxy(),
        nvim_rx,
    ));

    let mut app = app::App::new(config, fonts, nvim_tx, event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    // kill_on_drop takes the child down with the runtime
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    app.exit_result()
}
