"""Build and ad-hoc sign the native MacCanvas app using the pinned Rust toolchain."""
import json
from pathlib import Path
import plistlib
import shutil
import struct
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    if sys.platform != "darwin":
        raise SystemExit("Build this bundle on macOS.")
    if shutil.which("cargo") is None:
        raise SystemExit("Cargo is missing from PATH. Install rustup and load its environment before building.")
    command = ["cargo", "build", "--locked", "--release", "--bin", "maccanvas",
               "--message-format=json-render-diagnostics"]
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, text=True) as build:
        executable = icon = None
        for line in build.stdout:
            event = json.loads(line)
            if event["reason"] == "compiler-artifact" and event.get("executable") and event["target"]["name"] == "maccanvas":
                executable = Path(event["executable"])
            if event["reason"] == "build-script-executed":
                candidate = Path(event["out_dir"]) / "MacCanvas.png"
                if candidate.is_file():
                    icon = candidate
        if build.wait():
            raise SystemExit(build.returncode)
    if executable is None or icon is None:
        raise SystemExit("Cargo did not produce the macOS viewer and icon.")

    version = subprocess.check_output([executable, "--version"], text=True).strip().removeprefix("MacCanvas ")
    app = ROOT / "dist" / "MacCanvas.app"
    contents = app / "Contents"
    binary = contents / "MacOS" / "MacCanvas"
    resources = contents / "Resources"
    binary.parent.mkdir(parents=True, exist_ok=True)
    resources.mkdir(exist_ok=True)
    shutil.copy2(executable, binary)
    for notice in ["LICENSE", "THIRD_PARTY_NOTICES.txt"]:
        shutil.copy2(ROOT / notice, resources)
    # A PNG-backed 512-point Retina icon is an ic10 element in an ICNS container.
    png = icon.read_bytes()
    (resources / "MacCanvas.icns").write_bytes(
        struct.pack(">4sI4sI", b"icns", len(png) + 16, b"ic10", len(png) + 8) + png)
    with (contents / "Info.plist").open("wb") as info:
        plistlib.dump({
            "CFBundleIdentifier": "io.github.false00.MacCanvas",
            "CFBundleName": "MacCanvas",
            "CFBundleExecutable": binary.name,
            "CFBundleIconFile": "MacCanvas.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "NSHighResolutionCapable": True,
            "NSLocalNetworkUsageDescription": "Connect to Macs you select for screen sharing and file transfers.",
        }, info)
    subprocess.run(["codesign", "--force", "--sign", "-", app], check=True)
    subprocess.run(["codesign", "--verify", "--strict", app], check=True)
    subprocess.run([binary, "--version"], check=True)
    print(app)


if __name__ == "__main__":
    main()
