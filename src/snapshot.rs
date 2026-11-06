//! `--hidden` test mode: the window stays cloaked and never takes focus, and a
//! posted `WM_SNAPSHOT` saves the next frame as `%TEMP%\re_snap_<pid>_<wparam>.png`.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn path(id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("re_snap_{}_{id}.png", std::process::id()))
}

// alpha dropped, it's premultiplied when translucent
pub fn save(path: &std::path::Path, bgra: &[u8], width: u32, height: u32, stride: u32) -> Result<()> {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for row in bgra.chunks(stride as usize).take(height as usize) {
        for px in row[..(width * 4) as usize].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    image::save_buffer(path, &rgb, width, height, image::ColorType::Rgb8)
        .with_context(|| format!("saving {}", path.display()))
}

// RE_RECORD=<dir> dumps every presented frame as <ms>.png, for the README gif
pub struct Recorder {
    start: std::time::Instant,
    tx: std::sync::mpsc::Sender<(PathBuf, Vec<u8>, u32, u32, u32)>,
    dir: PathBuf,
}

impl Recorder {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("RE_RECORD")?);
        std::fs::create_dir_all(&dir).ok()?;
        let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, Vec<u8>, u32, u32, u32)>();
        std::thread::spawn(move || {
            for (path, bgra, w, h, stride) in rx {
                if let Err(err) = save(&path, &bgra, w, h, stride) {
                    log::warn!("record: {err:#}");
                }
            }
        });
        log::info!("recording frames to {}", dir.display());
        Some(Self { start: std::time::Instant::now(), tx, dir })
    }

    pub fn push(&self, bgra: Vec<u8>, w: u32, h: u32, stride: u32) {
        let ms = self.start.elapsed().as_millis();
        let _ = self.tx.send((self.dir.join(format!("{ms:07}.png")), bgra, w, h, stride));
    }
}

#[cfg(windows)]
mod hook {
    use std::sync::{Mutex, OnceLock};

    use windows::Win32::UI::WindowsAndMessaging::MSG;
    use winit::event_loop::{EventLoopBuilder, EventLoopProxy};
    use winit::platform::windows::EventLoopBuilderExtWindows;

    use crate::app::UserEvent;

    /// WM_APP + 'R'
    const WM_SNAPSHOT: u32 = 0x8000 + 0x52;

    // the hook goes on the builder, the proxy only exists after build()
    static PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();

    pub fn install(builder: &mut EventLoopBuilder<UserEvent>) {
        builder.with_msg_hook(|msg| {
            let msg = unsafe { &*(msg as *const MSG) };
            if msg.message != WM_SNAPSHOT {
                return false;
            }
            if let Some(proxy) = PROXY.get() {
                let _ = proxy
                    .lock()
                    .unwrap()
                    .send_event(UserEvent::Snapshot(msg.wParam.0 as u64));
            }
            true
        });
    }

    pub fn set_proxy(proxy: EventLoopProxy<UserEvent>) {
        let _ = PROXY.set(Mutex::new(proxy));
    }
}

#[cfg(windows)]
pub use hook::{install, set_proxy};
