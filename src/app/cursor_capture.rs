use std::cell::Cell;

use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::WindowsAndMessaging::{ClipCursor, GetClipCursor, GetForegroundWindow, GetWindowRect},
};

use super::{App, Error, Result, window_handle};

#[derive(Default)]
pub(super) struct CursorCapture {
    applied: Cell<Option<RECT>>,
}

impl CursorCapture {
    pub(super) fn sync(&self, app: &App) -> Result<()> {
        if !app.get_capture_cursor_fullscreen()
            || !app.get_immersive()
            || !app.get_connected()
            || app.get_modal_open()
        {
            return self.release().map_err(|e| Error::Display(e.to_string()));
        }
        let hwnd = HWND(window_handle(app)? as *mut _);
        self.confine_foreground(hwnd)
            .map_err(|e| Error::Display(e.to_string()))
    }

    fn confine_foreground(&self, hwnd: HWND) -> windows::core::Result<()> {
        if unsafe { GetForegroundWindow() } != hwnd {
            return self.release();
        }
        let mut bounds = RECT::default();
        let mut current = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut bounds)?;
            GetClipCursor(&mut current)?;
            if bounds != current && bounds.right > bounds.left && bounds.bottom > bounds.top {
                ClipCursor(Some(&bounds))?;
                self.applied.set(Some(bounds));
            }
        }
        Ok(())
    }

    fn release(&self) -> windows::core::Result<()> {
        if let Some(bounds) = self.applied.get() {
            let mut current = RECT::default();
            unsafe {
                GetClipCursor(&mut current)?;
                // Another window can take ownership of the system-wide clip.
                if current == bounds {
                    ClipCursor(None)?;
                }
            }
            self.applied.set(None);
        }
        Ok(())
    }
}

impl Drop for CursorCapture {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
