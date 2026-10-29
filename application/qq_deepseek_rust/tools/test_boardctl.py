import json
import unittest
import zlib
from collections import deque
from unittest.mock import patch
from boardctl import Board
from serial_protocol import Parser


class Port:
    def __init__(self, replies):
        self.replies = iter(replies); self.sent = []; self.input = bytearray()
    def write(self, data):
        self.sent.append(data)
        response = next(self.replies, None)
        if response is not None:
            obj = {"id":data.decode().split()[0][1:], **response}
            payload = json.dumps(obj).encode()
            self.input.extend(f"QD2_REPLY {len(payload):04x} {zlib.crc32(payload):08x} ".encode()+payload+b"\n")
    def flush(self): pass
    def read(self, count):
        data = self.input[:count];del self.input[:count];return bytes(data)


class BoardRetryTests(unittest.TestCase):
    def board(self, replies, boot="0123456789abcdef"):
        b=Board.__new__(Board);b.port=Port(replies);b.parser=Parser();b.parser.version=2;b.inbox=deque();b.evidence=None;b.sensitive=False;b.boot_id=boot;b.retries=0;return b
    def clock(self):
        value=0
        while True:
            value+=.25;yield value
    def test_lost_ack_retries_identical_id_and_boot_guard(self):
        b=self.board([None,{"ok":True,"result":{"persisted":True}}]);clock=self.clock()
        with patch('boardctl.time.monotonic',side_effect=lambda:next(clock)),patch('boardctl.time.sleep'):
            self.assertTrue(b.request('nvs test',timeout=3)['persisted'])
        self.assertEqual(b.port.sent[0],b.port.sent[1]);self.assertEqual(b.retries,1)
        self.assertIn(b'!0123456789abcdef nvs test',b.port.sent[0])
    def test_reboot_mismatch_is_not_retried(self):
        b=self.board([{"ok":False,"error":"device_restarted_request_not_replayed"}])
        with self.assertRaisesRegex(RuntimeError,'not_replayed'):b.request('nvs test')
        self.assertEqual(len(b.port.sent),1)
    def test_legacy_mutation_is_never_automatically_replayed(self):
        b=self.board([None],boot=None);clock=self.clock()
        with patch('boardctl.time.monotonic',side_effect=lambda:next(clock)),patch('boardctl.time.sleep'):
            with self.assertRaises(TimeoutError):b.request('nvs test',timeout=1)
        self.assertEqual(len(b.port.sent),1)


if __name__ == '__main__':unittest.main()
