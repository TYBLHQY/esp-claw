#!/usr/bin/env python3
"""Run the real share-link handler with stubbed HTTP and the firmware cJSON."""
import pathlib
import subprocess
import tempfile

here = pathlib.Path(__file__).resolve().parent
repo = here.parents[3]
cjson = repo / "application/edge_agent/managed_components/espressif__cjson/cJSON"
with tempfile.TemporaryDirectory(prefix="esp-qq-share-test-") as directory:
    exe = pathlib.Path(directory) / "test_qq_share"
    subprocess.run([
        "cc", "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-g",
        "-fsanitize=address,undefined", "-fno-omit-frame-pointer",
        "-I", str(here / "stubs"), "-I", str(here.parent / "src"),
        "-I", str(cjson), str(here / "test_qq_share.c"),
        str(here.parent / "src/cap_im_qq_share.c"), str(cjson / "cJSON.c"),
        "-lm", "-o", str(exe),
    ], check=True)
    subprocess.run([str(exe)], check=True)
