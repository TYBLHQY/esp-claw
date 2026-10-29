import json
import unittest
import zlib
from serial_protocol import Parser, request_frame


def frame(obj):
    payload = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode()
    return f"QD2_REPLY {len(payload):04x} {zlib.crc32(payload):08x} ".encode() + payload + b"\n"


class SerialFramingTests(unittest.TestCase):
    def test_every_fragment_size_and_split_utf8(self):
        obj = {"id": "abc", "result": "联网测试", "ok": True}
        encoded = frame(obj)
        for size in range(1, len(encoded) + 1):
            parser = Parser(); events = []
            for i in range(0, len(encoded), size):
                events += parser.feed(encoded[i:i + size])
            self.assertEqual(events, [(b"QD_REPLY ", obj)])
            self.assertEqual(parser.version, 2)

    def test_crc_corruption_and_next_frame_recovery(self):
        encoded = bytearray(frame({"id": "bad", "ok": True})); encoded[-4] ^= 1
        parser = Parser()
        events = parser.feed(encoded + frame({"id": "good", "ok": True}))
        self.assertEqual(events[0][1]["id"], "good")
        self.assertEqual(parser.crc_errors, 1)

    def test_log_prefix_suffix_and_legacy_are_accepted(self):
        obj = {"id": "legacy", "ok": True}
        parser = Parser()
        self.assertEqual(parser.feed(b"sdk log " + frame(obj).rstrip() + b" sdk suffix\r\n")[0][1], obj)
        old = b'QD_REPLY {"id":"legacy","ok":true} SDK LOG\r\n'
        self.assertEqual(Parser().feed(old)[0][1], obj)

    def test_truncated_or_overlong_frame_recovers(self):
        parser = Parser()
        self.assertEqual(parser.feed(b"QD2_REPLY ffff 00000000 {}\n"), [])
        self.assertEqual(parser.feed(frame({"ok": True}))[0][1], {"ok": True})
        parser.feed(b"x" * 100000)
        self.assertLessEqual(len(parser.pending), 24 * 1024)

    def test_command_crc_covers_exact_utf8_and_spaces(self):
        command = 'config import {"a":"空 格"}'
        result = request_frame("abc", command, 2)
        self.assertIn(f"#{zlib.crc32(command.encode()):08x} ".encode(), result)
        self.assertTrue(result.endswith(command.encode() + b"\n"))
        self.assertEqual(request_frame("abc", "status", 1), b"@abc status\n")


if __name__ == "__main__":
    unittest.main()
