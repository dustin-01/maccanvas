use slint::ComponentHandle;

slint::slint! {
    import { Button, GlassTheme } from "ui/glass.slint";

    export component ClipboardButtons inherits Window {
        title: "MacCanvas clipboard button test";
        width: 360px; height: 100px;
        background: #18181b;
        in-out property <bool> busy;
        in-out property <bool> simulate-transfer: true;
        out property <int> copies;
        out property <int> sends;
        callback transfer-started();
        callback focus-desktop();
        focus-desktop => { desktop.focus(); }
        init => {
            GlassTheme.dark = true;
            GlassTheme.motion = false;
            GlassTheme.effects = false;
        }
        Button {
            x: 10px; y: 20px; width: 150px; height: 40px;
            text: "Copy Mac"; icon: @image-url("assets/clipboard.svg");
            enabled: !root.busy;
            clicked => {
                root.copies += 1;
                if root.simulate-transfer {
                    root.busy = true;
                    desktop.focus();
                    root.transfer-started();
                }
            }
        }
        Button {
            x: 180px; y: 20px; width: 170px; height: 40px;
            text: "Send Windows"; icon: @image-url("assets/clipboard.svg");
            enabled: !root.busy;
            clicked => {
                root.sends += 1;
                if root.simulate-transfer {
                    root.busy = true;
                    desktop.focus();
                    root.transfer-started();
                }
            }
        }
        desktop := FocusScope { x: 0px; y: 70px; width: parent.width; height: 30px; }
        Text {
            x: 10px; y: 70px; height: 24px;
            text: "Copy actions: " + root.copies + " | Send actions: " + root.sends;
            color: white; font-size: 13px;
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = ClipboardButtons::new()?;
    let weak = ui.as_weak();
    ui.on_transfer_started(move || {
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_busy(false);
            }
        });
    });
    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::{
        LogicalPosition, Rgb8Pixel, SharedPixelBuffer,
        platform::{
            Platform, PointerEventButton, WindowAdapter, WindowEvent,
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        },
    };
    use std::rc::Rc;

    struct Headless(Rc<MinimalSoftwareWindow>);

    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn pointer(ui: &ClipboardButtons, x: f32, down: bool) {
        let position = LogicalPosition::new(x, 40.0);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(if down {
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            }
        } else {
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            }
        });
    }

    fn pixels(window: &MinimalSoftwareWindow) -> SharedPixelBuffer<Rgb8Pixel> {
        let mut pixels = SharedPixelBuffer::new(360, 100);
        window.request_redraw();
        assert!(window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), 360);
        }));
        pixels
    }

    fn focus_rings(window: &MinimalSoftwareWindow) -> (bool, bool) {
        let pixels = pixels(window);
        let accent = Rgb8Pixel::new(10, 132, 255);
        let focused = |x: usize| pixels.as_slice()[23 * 360 + x] == accent;
        (focused(80), focused(260))
    }

    fn pressed_buttons(window: &MinimalSoftwareWindow) -> (bool, bool) {
        let pixels = pixels(window);
        let pressed = |x: usize| pixels.as_slice()[54 * 360 + x].r > 65;
        (pressed(80), pressed(260))
    }

    #[test]
    fn clipboard_buttons_do_not_share_focus_press_or_activation() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
        let ui = ClipboardButtons::new().unwrap();
        ui.show().unwrap();
        window.set_size(slint::PhysicalSize::new(360, 100));
        window.dispatch_event(WindowEvent::WindowActiveChanged(true));
        pixels(&window);

        for x in [80.0, 260.0] {
            pointer(&ui, x, true);
            pointer(&ui, x, false);
            assert!(ui.get_busy());
            ui.set_busy(false);
        }
        assert_eq!((ui.get_copies(), ui.get_sends()), (1, 1));
        assert_eq!(focus_rings(&window), (false, false));

        ui.set_simulate_transfer(false);
        for (x, expected) in [(80.0, (true, false)), (260.0, (false, true))] {
            pointer(&ui, x, true);
            assert_eq!(pressed_buttons(&window), expected);
            pointer(&ui, x, false);
            assert_eq!(pressed_buttons(&window), (false, false));
            assert_eq!(focus_rings(&window), expected);
        }
        assert_eq!((ui.get_copies(), ui.get_sends()), (2, 2));

        window.dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
        assert_eq!(pressed_buttons(&window), (false, true));
        window.dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
        assert_eq!(pressed_buttons(&window), (false, false));
        assert_eq!((ui.get_copies(), ui.get_sends()), (2, 3));
        window.dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "\n".into() });
        assert_eq!((ui.get_copies(), ui.get_sends()), (2, 4));

        window.dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
        ui.set_busy(true);
        for x in [80.0, 260.0] {
            pointer(&ui, x, true);
            pointer(&ui, x, false);
        }
        window.dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
        window.dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "\n".into() });
        ui.set_busy(false);
        window.dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
        assert_eq!((ui.get_copies(), ui.get_sends()), (2, 4));
        assert_eq!(pressed_buttons(&window), (false, false));
        ui.invoke_focus_desktop();
        assert_eq!(focus_rings(&window), (false, false));
    }
}
