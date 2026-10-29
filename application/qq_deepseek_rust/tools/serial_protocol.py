"""Bounded diagnostic framing. Accept legacy firmware during migration."""
import json
import zlib

MAX_PAYLOAD = 24 * 1024
MARKERS = (b"QD2_REPLY ", b"QD2_EVENT ", b"QD_REPLY ", b"QD_EVENT ")


class Parser:
    def __init__(self):
        self.pending = bytearray()
        self.invalid_frames = 0
        self.crc_errors = 0
        self.version = 1

    def feed(self, data):
        self.pending.extend(data)
        if len(self.pending) > MAX_PAYLOAD * 3:
            self.pending = self.pending[-MAX_PAYLOAD:]
            self.invalid_frames += 1
        events = []
        while b"\n" in self.pending:
            raw, _, rest = self.pending.partition(b"\n")
            self.pending = bytearray(rest)
            for marker in MARKERS:
                offset = raw.find(marker)
                if offset < 0:
                    continue
                payload = raw[offset + len(marker):]
                try:
                    if marker.startswith(b"QD2_"):
                        length, checksum, payload = payload.split(b" ", 2)
                        length = int(length, 16)
                        if not (0 < length <= MAX_PAYLOAD) or len(payload.rstrip(b"\r")) < length:
                            raise ValueError("incomplete diagnostic frame")
                        payload = payload[:length]
                        if zlib.crc32(payload) != int(checksum, 16):
                            self.crc_errors += 1
                            raise ValueError("diagnostic checksum mismatch")
                        obj = json.loads(payload)
                        self.version = 2
                    else:
                        # A complete old JSON object followed by a C log is still usable.
                        obj, _ = json.JSONDecoder().raw_decode(payload.decode("utf-8"))
                    if not isinstance(obj, dict):
                        raise ValueError("diagnostic object required")
                except (ValueError, UnicodeError):
                    self.invalid_frames += 1
                    continue
                normalized = b"QD_REPLY " if b"REPLY" in marker else b"QD_EVENT "
                events.append((normalized, obj))
                break
        return events


def request_frame(request_id, command, version, boot_id=None):
    if boot_id:
        command = f"!{boot_id} {command}"
    if version >= 2:
        checksum = zlib.crc32(command.encode("utf-8"))
        command = f"#{checksum:08x} {command}"
    return ("@" + request_id + " " + command + "\n").encode("utf-8")
