#!/usr/bin/env python3
"""Run the built examples on Linux, each with its own launched browser."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def run_example(binary, arguments, directory):
    process = subprocess.Popen(
        [str(binary), *arguments], cwd=directory, start_new_session=True,
    )
    try:
        process.wait(timeout=60)
    except BaseException:
        # Kill only the process group created for this run, including its Chrome.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        raise
    if process.returncode != 0:
        raise RuntimeError(f"{binary.name} exited with status {process.returncode}")
    print(f"PASS {binary.name}", flush=True)


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
        with tempfile.TemporaryDirectory(prefix=f"rustcdp-{name}-") as directory:
            run_example(binaries / name, ["--headless=new", "--no-sandbox"], directory)


if __name__ == "__main__":
    main()
