//! Window geometry across runs, in `window.toml` next to `config.toml`. Physical px.

use std::path::PathBuf;

use crate::config::config_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    /// Outer position, inner size, both of the non-maximized window so that
    /// un-maximizing later lands in the right place.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

fn path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("window.toml"))
}

impl WindowState {
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(path()?).ok()?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Option<Self> {
        let table: toml::Table = text.parse().ok()?;
        let int = |key: &str| table.get(key)?.as_integer();
        let state = Self {
            x: int("x")?.try_into().ok()?,
            y: int("y")?.try_into().ok()?,
            width: int("width")?.try_into().ok()?,
            height: int("height")?.try_into().ok()?,
            maximized: table.get("maximized")?.as_bool()?,
        };
        (state.width >= 100 && state.height >= 100).then_some(state)
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let text = format!(
            "# Written by nvright on exit: window position and size.\n\
             x = {}\ny = {}\nwidth = {}\nheight = {}\nmaximized = {}\n",
            self.x, self.y, self.width, self.height, self.maximized
        );
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, text));
        if let Err(err) = result {
            log::warn!("cannot save {}: {err}", path.display());
        }
    }
}

/// Normal (restored) geometry straight from `GetWindowPlacement`. Correct even
/// while maximized; tracking resize events isn't, since they arrive before winit
/// reports the window as maximized.
#[cfg(windows)]
pub fn normal_placement(window: &winit::window::Window) -> Option<WindowState> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowPlacement, SW_SHOWMAXIMIZED, WINDOWPLACEMENT,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe { GetWindowPlacement(HWND(handle.hwnd.get() as *mut _), &mut placement) }.ok()?;
    let r = placement.rcNormalPosition;
    // rcNormalPosition is the outer rect. Subtract borders + title bar, measured
    // on the window as it is now.
    let (outer, inner) = (window.outer_size(), window.inner_size());
    let frame_w = outer.width.saturating_sub(inner.width);
    let frame_h = outer.height.saturating_sub(inner.height);
    Some(WindowState {
        x: r.left,
        y: r.top,
        width: ((r.right - r.left) as u32).saturating_sub(frame_w),
        height: ((r.bottom - r.top) as u32).saturating_sub(frame_h),
        maximized: placement.showCmd == SW_SHOWMAXIMIZED.0 as u32,
    })
}

/// DWM cloak: the window counts as visible and gets frames, DWM just doesn't
/// draw it. Hides the white rectangle at startup. Uncloaking waits for one
/// composition first so the first frame is already there.
/// false on other platforms or if the call fails.
#[cfg(windows)]
pub fn set_cloaked(window: &winit::window::Window, cloaked: bool) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmFlush, DwmSetWindowAttribute};
    use windows::core::BOOL;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(RawWindowHandle::Win32(handle)) = window.window_handle().ok().map(|h| h.as_raw())
    else {
        return false;
    };
    if !cloaked {
        // let the first presented frame reach the compositor
        let _ = unsafe { DwmFlush() };
    }
    let value = BOOL::from(cloaked);
    let result = unsafe {
        DwmSetWindowAttribute(
            HWND(handle.hwnd.get() as *mut _),
            DWMWA_CLOAK,
            &value as *const BOOL as *const _,
            size_of::<BOOL>() as u32,
        )
    };
    result
        .inspect_err(|err| log::warn!("DWMWA_CLOAK {cloaked}: {err}"))
        .is_ok()
}

#[cfg(not(windows))]
pub fn set_cloaked(_window: &winit::window::Window, _cloaked: bool) -> bool {
    false
}

/// Acrylic behind the translucent parts. DWM only draws the system backdrop
/// in the frame, so the frame gets extended over the whole client area, and
/// without dark mode the tint is white.
#[cfg(windows)]
pub fn set_blur(window: &winit::window::Window, on: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWM_SYSTEMBACKDROP_TYPE, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_USE_IMMERSIVE_DARK_MODE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
    };
    use windows::Win32::UI::Controls::MARGINS;
    use windows::core::BOOL;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(RawWindowHandle::Win32(handle)) = window.window_handle().ok().map(|h| h.as_raw())
    else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);
    let edge = if on { -1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: edge,
        cxRightWidth: edge,
        cyTopHeight: edge,
        cyBottomHeight: edge,
    };
    let dark = BOOL::from(true);
    let backdrop: DWM_SYSTEMBACKDROP_TYPE = if on {
        DWMSBT_TRANSIENTWINDOW
    } else {
        DWMSBT_NONE
    };
    let result = unsafe {
        DwmExtendFrameIntoClientArea(hwnd, &margins).and_then(|()| {
            if on {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_USE_IMMERSIVE_DARK_MODE,
                    &dark as *const BOOL as *const _,
                    size_of::<BOOL>() as u32,
                )?;
            }
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop as *const DWM_SYSTEMBACKDROP_TYPE as *const _,
                size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
            )
        })
    };
    if let Err(err) = result {
        log::warn!("blur {on}: {err} (needs Windows 11 22H2)");
    }
}

#[cfg(not(windows))]
pub fn set_blur(_window: &winit::window::Window, _on: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_state() {
        let s =
            WindowState::parse("x = -1200\ny = 40\nwidth = 1400\nheight = 900\nmaximized = true\n");
        assert_eq!(
            s,
            Some(WindowState {
                x: -1200,
                y: 40,
                width: 1400,
                height: 900,
                maximized: true
            })
        );
        assert_eq!(WindowState::parse("x = 1"), None);
        assert_eq!(
            WindowState::parse("x = 0\ny = 0\nwidth = 10\nheight = 10\nmaximized = false"),
            None
        );
    }
}
