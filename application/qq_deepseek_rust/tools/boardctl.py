#!/usr/bin/env python3
"""Local UART provisioning and diagnostics. Never print transmitted secrets.

Use the ESP-IDF Python environment (which already supplies pyserial).
Logs contain only parsed firmware diagnostics, never raw received lines.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import time
import uuid
from collections import deque

import serial
from serial_protocol import Parser, request_frame


class Board:
    def __init__(self, port, evidence=None):
        self.port = serial.Serial(port=None, baudrate=115200, timeout=0.1,
                                  write_timeout=5, exclusive=True)
        self.port.dtr = False
        self.port.rts = False
        self.port.port = port
        self.port.open()
        self.parser = Parser()
        self.inbox = deque()
        self.evidence = evidence
        self.sensitive = False
        self.boot_id = None
        self.retries = 0

    def close(self):
        self.port.close()

    def record(self, obj):
        if self.evidence and not self.sensitive:
            with self.evidence.open("a", encoding="utf-8") as out:
                out.write(json.dumps({"host_time": time.time(), **obj}, ensure_ascii=False) + "\n")

    def messages(self, deadline):
        while time.monotonic() < deadline:
            if not self.inbox:
                self.inbox.extend(self.parser.feed(self.port.read(256)))
            while self.inbox:
                marker, obj = self.inbox.popleft()
                self.record({"kind": marker.decode().strip(), "data": obj})
                yield marker, obj

    def request(self, command, timeout=12):
        self.sensitive = command.strip() == "web pairing"
        request_id = uuid.uuid4().hex[:12]
        payload = request_frame(request_id, command, self.parser.version, self.boot_id)
        if len(payload) > 6144:
            raise RuntimeError("UART command exceeds bounded input")
        readonly = command.strip() in {"status", "led status", "wifi status", "bot status", "deepseek status", "sd status", "help"}
        # Mutations can only be retried when this firmware has advertised a boot
        # guard and response cache. A reboot rejects the old guard before execution.
        attempts = 3 if self.boot_id or readonly else 1
        for attempt in range(attempts):
            if attempt:
                self.retries += 1
            for offset in range(0, len(payload), 128):
                self.port.write(payload[offset:offset + 128])
                time.sleep(0.003)
            self.port.flush()
            for marker, obj in self.messages(time.monotonic() + timeout / attempts):
                if marker == b"QD_REPLY " and obj.get("id") == request_id:
                    if not obj.get("ok"):
                        if obj.get("error") == "request_checksum_mismatch" and attempt + 1 < attempts:
                            break
                        raise RuntimeError("device rejected command: " + str(obj.get("error")))
                    result = obj["result"]
                    console = result.get("console", {}) if isinstance(result, dict) else {}
                    if console.get("protocol") == 3:
                        self.boot_id = console.get("boot_id")
                    if isinstance(result, dict) and result.get("rebooting"):
                        self.boot_id = None
                    return result
        raise TimeoutError(f"No matching firmware reply after {attempts} safe protocol attempt(s); invalid_frames={self.parser.invalid_frames}, crc_errors={self.parser.crc_errors}; command was not logged; verify state before issuing a new request")

    def identify(self):
        self.boot_id = None
        result = self.request("status")
        if result.get("hardware", {}).get("board") != "GOOUUU ESP32-S3-CAM N16R8":
            raise RuntimeError("unexpected firmware identity; refusing provisioning")
        return result

    def reset(self):
        self.boot_id = None
        self.port.dtr = False
        self.port.rts = True
        time.sleep(0.15)
        self.port.rts = False
        # No BOOT assertion. This only resets into the selected application.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", default="/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0")
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--config", type=Path, help="private JSON file to provision after firmware identification")
    parser.add_argument("--reset", action="store_true")
    parser.add_argument("--wait-online", type=int, default=0)
    parser.add_argument("--led-test", action="store_true")
    parser.add_argument("commands", nargs="*", default=[])
    args = parser.parse_args()
    if args.evidence:
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        args.evidence.touch(mode=0o600, exist_ok=True)
        os.chmod(args.evidence, 0o600)
    board = Board(args.port, args.evidence)
    try:
        if args.reset:
            board.reset()
            time.sleep(3)
        result = board.identify()
        if args.config:
            if args.config.stat().st_mode & 0o077:
                raise RuntimeError("provisioning file must be private (chmod 600)")
            private = json.loads(args.config.read_text(encoding="utf-8"))
            result = board.request("config import " + json.dumps(private, separators=(",", ":")))
            print(json.dumps({"provision": result}, ensure_ascii=False))
            del private
        for command in args.commands:
            if command.lstrip().startswith("config"):
                raise RuntimeError("use --config to avoid credentials in command-line arguments")
            result = board.request(command)
            print(json.dumps({"command": command, "result": result}, ensure_ascii=False))
        if args.led_test:
            for color, values in (("red", "16 0 0"), ("green", "0 16 0"), ("blue", "0 0 16")):
                result = board.request("led rgb " + values)
                print(json.dumps({"visual_test_color": color, "result": result}, ensure_ascii=False))
                time.sleep(3)
            result = board.request("led off")
            print(json.dumps({"visual_test_color": "off", "result": result}, ensure_ascii=False))
        if args.wait_online:
            deadline = time.monotonic() + args.wait_online
            while time.monotonic() < deadline:
                result = board.request("wifi status")
                if result.get("online") and result.get("time_synchronized"):
                    print(json.dumps({"wifi_ready": result}, ensure_ascii=False))
                    break
                time.sleep(1)
            else:
                raise TimeoutError("Wi-Fi/TLS clock not ready: " + json.dumps(result))
        if not args.commands and not args.config and not args.led_test and not args.wait_online:
            print(json.dumps(result, ensure_ascii=False))
    finally:
        board.close()


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, TimeoutError, serial.SerialException) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1)
