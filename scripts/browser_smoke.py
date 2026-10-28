#!/usr/bin/env python3
"""Run the built examples on Linux, including external-browser ownership checks."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def run_example(binary, arguments, directory, expected_success=True):
    process = subprocess.Popen(
        [str(binary), *arguments], cwd=directory, start_new_session=True,
        stderr=subprocess.PIPE if not expected_success else None,
    )
    try:
        _, error = process.communicate(timeout=60)
    except BaseException:
        # Kill only the process group created for this run, including its Chrome.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        raise
    if expected_success:
        if process.returncode != 0:
            raise RuntimeError(f"{binary.name} exited with status {process.returncode}")
    elif process.returncode != 1 or b"requested browser tab was not found" not in error:
        raise RuntimeError(f"unexpected failure: {process.returncode}, {error!r}")
    print(f"PASS {binary.name}", flush=True)


def get_json(origin, path):
    with urllib.request.urlopen(origin + path, timeout=5) as response:
        return json.load(response)


def external_browser(chrome, binary):
    with tempfile.TemporaryDirectory(prefix="rustcdp-external-") as directory:
        profile = Path(directory) / "profile"
        with open(Path(directory) / "chrome.log", "w+") as log:
            process = subprocess.Popen(
                [chrome, "--headless=new", "--no-sandbox", "--remote-debugging-port=0",
                 f"--user-data-dir={profile}", "about:blank"],
                stdout=log, stderr=log, start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 20
                while True:
                    try:
                        port, _ = (profile / "DevToolsActivePort").read_text().splitlines()[:2]
                        origin = f"http://127.0.0.1:{port}"
                        version = get_json(origin, "/json/version")
                        original = {page["id"] for page in get_json(origin, "/json/list")
                                    if page["type"] == "page"}
                        if not original:
                            raise ValueError("waiting for Chrome's initial tab")
                        break
                    except (OSError, ValueError):
                        if process.poll() is not None or time.monotonic() >= deadline:
                            log.seek(0)
                            raise RuntimeError(f"Chrome startup failed:\n{log.read()}")
                        time.sleep(0.05)
                cases = [([origin, sorted(original)[0]], True),
                         ([version["webSocketDebuggerUrl"]], True),
                         ([origin, "missing-target"], False)]
                for arguments, success in cases:
                    run_example(binary, arguments, directory, expected_success=success)
                    if process.poll() is not None:
                        raise RuntimeError("connect terminated external Chrome")
                    get_json(origin, "/json/version")
                    remaining = {page["id"] for page in get_json(origin, "/json/list")
                                 if page["type"] == "page"}
                    if remaining != original:
                        raise RuntimeError("connect left a temporary tab or closed an original tab")
                print("PASS external Chrome and original tabs survive", flush=True)
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()


def main():
    chrome = os.environ.get("CHROME_PATH") or next(
        (path for name in ["google-chrome", "chromium", "chromium-browser", "chrome"]
         if (path := shutil.which(name))), None
    )
    if not chrome:
        raise RuntimeError("set CHROME_PATH to the Chrome executable")
    chrome = str(Path(chrome).resolve())
    os.environ["CHROME_PATH"] = chrome
    subprocess.run([chrome, "--version"], check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"], cwd=ROOT
    ))
    binaries = Path(metadata["target_directory"]) / "debug" / "examples"
    manifest = tomllib.loads((ROOT / "examples" / "Cargo.toml").read_text())
    for example in manifest["example"]:
        name = example["name"]
        if name == "connect":
            continue
        with tempfile.TemporaryDirectory(prefix=f"rustcdp-{name}-") as directory:
            run_example(binaries / name, ["--headless=new", "--no-sandbox"], directory)
    external_browser(chrome, binaries / "connect")


if __name__ == "__main__":
    main()
