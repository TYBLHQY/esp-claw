#!/usr/bin/env python3
"""Copy one small image into the SD owner's media directory over guarded UART.

Requires a stopped bot. Never overwrites a completed image or a partial transfer.
No image bytes, credentials, or signed URLs are logged.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import time
from boardctl import Board

LIMIT = 512 * 1024
CHUNK = 1024

def wait_sd(board):
    end = time.monotonic() + 8
    while time.monotonic() < end:
        state = board.request("sd status")
        state = state.get("sd", state)
        if not state.get("busy"):
            if not state.get("available") or state.get("fault"):
                raise RuntimeError("SD operation did not finish successfully")
            return state.get("last_result", {})
        time.sleep(0.05)
    raise TimeoutError("SD deadline; inspect state before any new write")

def stage(board, source, name):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", name) or ".." in name or Path(name).suffix.lower() not in {".png", ".jpg", ".jpeg", ".gif", ".webp"}:
        raise ValueError("use an ASCII image filename of at most 64 bytes")
    before = source.stat()
    if not 0 < before.st_size <= LIMIT:
        raise ValueError("image must be between 1 byte and 512 KiB")
    status = board.identify()
    if tuple(int(v) for v in status["hardware"]["firmware"].split(".")[:3]) < (0, 7, 0):
        raise RuntimeError("firmware 0.7.0 or later required")
    bot = status["services"].get("bot") or {}
    if bot.get("active_workers", 0) or status["services"].get("diagnostic_running"):
        raise RuntimeError("stop the bot and finish diagnostics before SD import")
    if not status["sd"].get("available"):
        raise RuntimeError("SD is not available")
    md5, sha1, offset = hashlib.md5(), hashlib.sha1(), 0
    with source.open("rb") as stream:
        while chunk := stream.read(CHUNK):
            if offset + len(chunk) > before.st_size:
                raise RuntimeError("source changed during transfer; partial file retained")
            board.request(f"sd media put {name} {offset} {chunk.hex()}")
            wait_sd(board)
            md5.update(chunk)
            sha1.update(chunk)
            offset += len(chunk)
    after = source.stat()
    if offset != before.st_size or after.st_size != before.st_size or after.st_mtime_ns != before.st_mtime_ns:
        raise RuntimeError("source changed; partial file retained without publishing")
    board.request(f"sd media finish {name} {offset}")
    wait_sd(board)
    board.request(f"sd media info {name}")
    result = wait_sd(board)
    if result.get("md5") != md5.hexdigest() or result.get("sha1") != sha1.hexdigest():
        raise RuntimeError("SD digest mismatch; image was not uploaded to QQ")
    return {"name": name, "bytes": offset, "md5": md5.hexdigest(), "sha1": sha1.hexdigest(), "verified": True}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument("--name")
    parser.add_argument("--port", default="/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0")
    args = parser.parse_args()
    board = Board(args.port)
    try:
        print(json.dumps(stage(board, args.image, args.name or args.image.name), ensure_ascii=False))
    finally:
        board.close()

if __name__ == "__main__":
    main()
