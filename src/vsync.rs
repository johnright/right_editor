//! Per-refresh tick for animations. A thread blocks in `DwmFlush` (once per
//! composition) and pokes the window, so the window thread never waits on
//! vsync itself. Parked when nothing is animating.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, Thread};
use std::time::Duration;

use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

pub struct VSync {
    active: Arc<AtomicBool>,
    /// Only used when there's no DwmFlush.
    interval_us: Arc<AtomicU64>,
    thread: Thread,
}

impl VSync {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> std::io::Result<Self> {
        let active = Arc::new(AtomicBool::new(false));
        let interval_us = Arc::new(AtomicU64::new(16_667));
        let handle = {
            let active = active.clone();
            let interval_us = interval_us.clone();
            thread::Builder::new().name("vsync".into()).spawn(move || {
                loop {
                    if !active.load(Ordering::Acquire) {
                        thread::park();
                        continue;
                    }
                    wait_for_refresh(Duration::from_micros(interval_us.load(Ordering::Relaxed)));
                    if proxy.send_event(UserEvent::VSync).is_err() {
                        return; // window closed
                    }
                }
            })?
        };
        Ok(Self {
            active,
            interval_us,
            thread: handle.thread().clone(),
        })
    }

    pub fn set_active(&self, active: bool) {
        if self.active.swap(active, Ordering::AcqRel) != active && active {
            self.thread.unpark();
        }
    }

    pub fn set_interval(&self, interval: Duration) {
        self.interval_us
            .store(interval.as_micros() as u64, Ordering::Relaxed);
    }
}

#[cfg(windows)]
fn wait_for_refresh(fallback: Duration) {
    // returns at the next composition; sleep instead if DWM isn't there
    if unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.is_err() {
        thread::sleep(fallback);
    }
}

#[cfg(not(windows))]
fn wait_for_refresh(interval: Duration) {
    thread::sleep(interval);
}
