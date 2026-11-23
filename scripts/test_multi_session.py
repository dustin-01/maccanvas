"""Exercise two simultaneous Mac connections using the real viewer and loopback RFB servers."""

import argparse
import ctypes
from ctypes import wintypes
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from test_gui import ROOT, Fixture, automate, native_window, pixel, read, user32, wait


def fixture_color(fixture):
    # GetPixel returns 0x00BBGGRR. The fixture sends B, G, R, padding.
    return int.from_bytes(fixture.solid_bgra[:3], "big")


def check_background_preview(process, window, fixture, name):
    preview = automate(process.pid, fixture.port, "preview-bounds", ("-MachineName", name))
    assert preview["width"] > 0 and preview["height"] > 0, preview
    wait(lambda: pixel(window, preview["x"] + preview["width"] / 2,
                       preview["y"] + preview["height"] / 2) == fixture_color(fixture),
         process, fixture, f"{name} sidebar thumbnail")
    return preview


def click_in_viewer(window, point):
    if not user32.SetPhysicalCursorPos(point.x, point.y):
        raise ctypes.WinError(ctypes.get_last_error())
    if not user32.ScreenToClient(window, ctypes.byref(point)):
        raise ctypes.WinError(ctypes.get_last_error())
    coordinates = (point.y << 16) | (point.x & 0xffff)
    for message, buttons in ((0x200, 0), (0x201, 1), (0x202, 0)):
        if not user32.PostMessageW(window, message, buttons, coordinates):
            raise ctypes.WinError(ctypes.get_last_error())


def check_fullscreen_slide(process, window, fixture, scale):
    bounds = automate(process.pid, fixture.port, "bounds")
    # At rest the graph begins 12 logical pixels below the edge. Its panel
    # crosses this otherwise black pixel only while sliding offscreen.
    x, y = bounds["x"] + 45 * scale, bounds["y"] + 4 * scale
    motion = wintypes.BOOL()
    if not user32.SystemParametersInfoW(0x1042, 0, ctypes.byref(motion), 0):
        raise ctypes.WinError(ctypes.get_last_error())
    assert pixel(window, x, y) == 0
    user32.SetPhysicalCursorPos(round(bounds["x"] + bounds["width"] / 2),
                                round(bounds["y"] + bounds["height"] / 2))
    seen = set()
    deadline = time.monotonic() + 0.6
    while time.monotonic() < deadline:
        seen.add(pixel(window, x, y))
        time.sleep(0.005)
    assert pixel(window, x, y) == 0
    assert bool(seen - {0}) == bool(motion.value), f"Unexpected fullscreen slide: {seen}"


def switch_card_and_check_transition(process, window, outgoing, incoming, preview):
    motion = wintypes.BOOL()
    if not user32.SystemParametersInfoW(0x1042, 0, ctypes.byref(motion), 0):  # SPI_GETCLIENTAREAANIMATION
        raise ctypes.WinError(ctypes.get_last_error())
    bounds = automate(process.pid, outgoing.port, "bounds")
    x = round(bounds["x"] + bounds["width"] / 2)
    y = round(bounds["y"] + bounds["height"] / 2)
    old, new = fixture_color(outgoing), fixture_color(incoming)
    assert pixel(window, x, y) == old, "The outgoing desktop must be visible before switching"
    channels = list(zip(old.to_bytes(3, "little"), new.to_bytes(3, "little")))
    samples = set()
    blended = False
    click_in_viewer(window, wintypes.POINT(round(preview["x"] + preview["width"] / 2),
                                          round(preview["y"] + preview["height"] / 2)))
    deadline = time.monotonic() + 12
    observed = old
    while time.monotonic() < deadline:
        observed = pixel(window, x, y)
        samples.add(observed)
        if observed <= 0xffffff:
            # Both fixture colors differ in every channel. Require a color
            # between both endpoints in all three, allowing for renderer gamma.
            blended |= all(min(a, b) + 3 <= value <= max(a, b) - 3
                           for (a, b), value in zip(channels, observed.to_bytes(3, "little")))
        if observed == new and (not motion.value or blended):
            break
        time.sleep(0.005)
    assert observed == new, f"The card switch did not reach the selected Mac's frame: {samples}"
    assert blended == bool(motion.value), f"Unexpected transition with Windows animation={motion.value}: {samples}"
    print("PASS: card switch blended both desktop colors" if motion.value else
          "PASS: card switch respected reduced motion")


def cancel_pending_mac(process):
    name = "Pending Mac"
    with socket.create_server(("127.0.0.1", 0)) as listener:
        listener.settimeout(10)
        port = listener.getsockname()[1]
        automate(process.pid, port, "connect-another", ("-MachineName", name))
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(10)
            connection.sendall(b"RFB 003.889\n")
            assert read(connection, 12) == b"RFB 003.008\n"
            # Hold the security-scheme offer until the user cancels this card.
            automate(process.pid, port, "cancel-pending-card", ("-MachineName", name))
            try:
                assert connection.recv(1) == b"", "Cancelling must close the pending connection"
            except ConnectionResetError:
                pass


def check_desktop_and_input(process, window, active, other, letter):
    bounds = automate(process.pid, active.port, "bounds")
    width, height = active.size
    scale = min(bounds["width"] / width, bounds["height"] / height, 1)
    location = wintypes.POINT(
        round(bounds["x"] + bounds["width"] / 2 + width * scale / 4),
        round(bounds["y"] + bounds["height"] / 2 - height * scale / 4),
    )
    wait(lambda: pixel(window, location.x, location.y) == fixture_color(active),
         process, active, "the selected Mac's frame")
    before = len(active.pointers)
    other_before = len(other.pointers)
    click_in_viewer(window, location)
    key = ord(letter.upper())
    key_data = (user32.MapVirtualKeyW(key, 0) << 16) | 1
    for message, parameter, data in (
        (0x100, key, key_data), (0x102, ord(letter), key_data),
        (0x101, key, key_data | 0xc0000000),
    ):
        if not user32.PostMessageW(window, message, parameter, data):
            raise ctypes.WinError(ctypes.get_last_error())
    wait(lambda: (1, ord(letter)) in active.keys and (0, ord(letter)) in active.keys,
         process, active, "the selected Mac's keyboard response")
    assert any(mask == 1 and abs(x - width * 0.75) <= 2 and abs(y - height * 0.25) <= 2
               for mask, x, y in active.pointers[before:]), active.pointers[before:]
    assert not any(mask & 1 for mask, _, _ in other.pointers[other_before:])
    assert (1, ord(letter)) not in other.keys
    return bounds


def run(executable):
    first = Fixture(size=(320, 240), solid_bgra=b"\x30\x58\xe0\0")
    second = Fixture(size=(400, 300), solid_bgra=b"\xd8\x90\x20\0")
    first_name, second_name = "Studio Mac", "Office Mac"
    threads = []
    original = wintypes.POINT()
    if not user32.GetPhysicalCursorPos(ctypes.byref(original)):
        raise ctypes.WinError(ctypes.get_last_error())
    with tempfile.TemporaryDirectory(prefix="maccanvas-multi-") as directory:
        process = subprocess.Popen([str(executable), "--data-dir", directory],
                                   stderr=subprocess.PIPE, text=True, creationflags=subprocess.CREATE_NO_WINDOW)
        window = None
        try:
            window = wait(lambda: native_window(process.pid), process, first, "viewer window")
            for fixture, name, action in ((first, first_name, "connect"),
                                          (second, second_name, "connect-another")):
                thread = threading.Thread(target=fixture.run, daemon=True)
                thread.start()
                threads.append(thread)
                automate(process.pid, fixture.port, action, ("-MachineName", name))
                wait(fixture.authenticated.is_set, process, fixture, "loopback Mac authentication")
                check_desktop_and_input(process, window, fixture,
                                        second if fixture is first else first,
                                        "a" if fixture is first else "b")

            # Change the hidden Mac after the switch to distinguish a live preview
            # from an image cached when it last held keyboard focus.
            first.solid_bgra = b"\x90\x30\xe0\0"
            preview = check_background_preview(process, window, first, first_name)
            switch_card_and_check_transition(process, window, second, first, preview)
            expanded = check_desktop_and_input(process, window, first, second, "c")
            second.solid_bgra = b"\xc0\xc8\x18\0"
            check_background_preview(process, window, second, second_name)

            automate(process.pid, first.port, "toggle-sidebar")
            wait(lambda: automate(process.pid, first.port, "bounds")["width"] > expanded["width"] + 200,
                 process, first, "collapsed sidebar")
            automate(process.pid, second.port, "select-switcher", ("-MachineName", second_name))
            collapsed = check_desktop_and_input(process, window, second, first, "d")
            assert collapsed["width"] > expanded["width"] + 200
            assert not first.disconnected.is_set() and not second.disconnected.is_set()

            automate(process.pid, second.port, "fullscreen")
            picker = automate(process.pid, second.port, "reveal-fullscreen")
            scale = picker["scale"]
            x = round(picker["x"] + picker["width"] / 2)
            y = round(picker["y"] + picker["height"] / 2)
            click_in_viewer(window, wintypes.POINT(x, y))
            # Move onto the second row, below the reveal band. The popup must
            # remain painted there until the user chooses a Mac or dismisses it.
            row_one = round(picker["y"] + picker["height"] + 20 * scale)
            row_two = round(row_one + 28 * scale)
            user32.SetPhysicalCursorPos(x, row_two)
            time.sleep(0.4)
            assert pixel(window, x, row_two) not in (0, 0xffffffff), "Fullscreen picker popup disappeared"
            click_in_viewer(window, wintypes.POINT(x, row_one))
            check_desktop_and_input(process, window, first, second, "g")
            picker = automate(process.pid, first.port, "reveal-fullscreen")
            check_fullscreen_slide(process, window, first, picker["scale"])
            automate(process.pid, first.port, "reveal-fullscreen")
            automate(process.pid, first.port, "exit-fullscreen")
            automate(process.pid, first.port, "toggle-sidebar")
            second.expect_disconnect.set()
            automate(process.pid, second.port, "disconnect-card", ("-MachineName", second_name))
            wait(second.disconnected.is_set, process, second, "the background Mac's socket to close")
            check_desktop_and_input(process, window, first, second, "e")
            automate(process.pid, second.port, "check-card-actions", ("-MachineName", second_name))
            cancel_pending_mac(process)
            check_desktop_and_input(process, window, first, second, "f")
            assert not first.disconnected.is_set()
            assert second.error is None and first.error is None
            assert all((1, ord(k)) not in second.keys for k in "acef")
            assert all((1, ord(k)) not in first.keys for k in "bd")
            print("PASS: fullscreen hover delay, sliding controls and Mac picker, "
                  "independent live previews, card and collapsed-header switching, "
                  "isolated input, surviving connection after disconnect, and cancelling a pending Mac")
        finally:
            user32.SetPhysicalCursorPos(original.x, original.y)
            for fixture in (first, second):
                fixture.stop.set()
                fixture.listener.close()
            if window and process.poll() is None:
                user32.PostMessageW(window, 0x10, 0, 0)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise
            for thread in threads:
                thread.join(timeout=3)
            if process.returncode:
                raise AssertionError(f"Viewer exit code {process.returncode}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", type=Path,
                        default=ROOT / "target" / "x86_64-pc-windows-msvc" / "debug" / "maccanvas.exe")
    run(parser.parse_args().executable.resolve())
