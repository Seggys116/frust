#!/usr/bin/env python3
"""Checks the trace barriers of darwin-write-probe.py against a fake fs_usage that runs on any host."""

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location("darwin_writes", Path(__file__).with_name("darwin-write-probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

DEVICES = ["/dev/disk9", "/dev/disk9s1"]
DATA = "/dev/disk9s1"
# Prints a data line and a container metadata line for each stable new file, like fs_usage -f diskio.
FAKE = """
import os, signal, sys, time
root, mode, start, delay, drop = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4]), sys.argv[5] == "drop"
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
launched = time.monotonic()
sizes, emitted, queued = {}, set(), []
while True:
    now = time.monotonic()
    for base, _, names in os.walk(root):
        for name in names:
            path = os.path.join(base, name)
            if path in emitted:
                continue
            try:
                size = os.stat(path).st_size
            except OSError:
                continue
            if size and sizes.get(path) == size:
                emitted.add(path)
                if now - launched >= start and not (drop and ".frust-marker" not in name):
                    queued.append((now + delay, path, (size + 4095) // 4096 * 4096))
            sizes[path] = size
    while queued and queued[0][0] <= now:
        _, path, size = queued.pop(0)
        stamp = time.strftime("%H:%M:%S") + ".000001"
        text = " " + path if mode == "token" else ""
        print(f"{stamp}  WrData[A]  D=0x00000010  B=0x{size:x}  /dev/disk9s1{text}  0.000050 W kernel_task.1", flush=True)
        print(f"{stamp}  WrMeta[A]  D=0x00000020  B=0x2000  /dev/disk9  0.000050 W kernel_task.1", flush=True)
    time.sleep(0.02)
"""


def blocks(mount):
    return sum(path.stat().st_blocks * 512 for path in Path(mount).rglob("*") if path.is_file())


class Fixture:
    def __init__(self, test, mode="token", start=0.0, delay=0.0, drop=False, command=None):
        self.scratch = tempfile.TemporaryDirectory()
        test.addCleanup(self.scratch.cleanup)
        self.root, self.mount = Path(self.scratch.name) / "root", Path(self.scratch.name) / "mount"
        self.root.mkdir()
        self.mount.mkdir()
        argv = command or [sys.executable, "-I", "-c", FAKE, str(self.mount), mode, str(start), str(delay),
                           "drop" if drop else "keep"]
        for patch in (mock.patch.object(probe, "tracer_command", lambda: argv),
                      mock.patch.object(probe, "available_bytes", lambda mount: (1 << 40) - blocks(mount)),
                      mock.patch.object(probe, "MARKER_RETRY_SECONDS", 0.3),
                      mock.patch.object(probe, "MARKER_LIMIT_SECONDS", 3),
                      mock.patch.object(probe, "SETTLE_SECONDS", 0.2),
                      mock.patch.object(probe, "QUIET_SECONDS", 0.3),
                      mock.patch.object(probe, "LEGACY_TAIL_SECONDS", 0.5)):
            patch.start()
            test.addCleanup(patch.stop)

    def action(self, files=3, size=8192):
        def run():
            for index in range(files):
                (self.mount / f"data{index}").write_bytes(b"x" * size)
        return run

    def sample(self, action=None):
        return probe.sample(self.root, self.mount, DEVICES, "phase", action or self.action())


class WriteRecord(unittest.TestCase):
    LINE = "12:00:00.000001  {kind}[A]  D=0x00000010  B=0x{size}  {device}  /x  0.000050 W kernel_task.1"

    def test_reads_each_write_kind(self):
        for kind in probe.KINDS:
            line = self.LINE.format(kind=kind, size="5000", device=DATA)
            self.assertEqual(probe.write_record(line), (kind, 0x5000, DATA))

    def test_buffer_cache_marker_is_skipped(self):
        line = self.LINE.format(kind="WrData", size="5000", device=DATA).replace("B=0x5000", "B=0x5000 BC:HIT")
        self.assertEqual(probe.write_record(line), ("WrData", 0x5000, DATA))

    def test_ignores_other_calls(self):
        self.assertIsNone(probe.write_record("12:00:00.000001  open  /tmp/x  0.000010 W python3.1"))

    def test_ignores_unresolved_subsector_device_writes(self):
        line = self.LINE.format(kind="WrData", size="5c", device="/dev/NOTFOUND")
        self.assertIsNone(probe.write_record(line.replace(" /x ", " /dev/null ")))
        self.assertIsNone(probe.write_record(line.replace(" /x ", " ")))

    def test_rejects_unnamed_device_and_failed_writes(self):
        with self.assertRaises(probe.TraceError):
            probe.write_record(self.LINE.format(kind="WrData", size="5000", device="/dev/NOTFOUND"))
        with self.assertRaises(probe.TraceError):
            probe.write_record("12:00:00.000001  WrData[A]  D=0x00000010  [  5] /x 0.000050 W kernel_task.1")

    def test_rejects_tracer_messages(self):
        for line in ("fs_usage: buffer overrun, events generated too quickly", "sudo: a password is required"):
            with self.assertRaises(probe.TraceError):
                probe.write_record(line)


class ImageDetach(unittest.TestCase):
    def test_busy_image_can_already_be_unmounted(self):
        mount = Path("/empty/validation-volume")
        attached = {"system-entities": [{"dev-entry": device} for device in DEVICES]}
        busy = probe.ProbeError("resource busy")
        with (mock.patch.object(Path, "is_mount", return_value=False),
              mock.patch.object(probe, "locate_image", return_value=attached),
              mock.patch.object(probe, "command", side_effect=[busy, b""]) as command):
            probe.detach_image(Path("validation.sparseimage"), DEVICES[0], mount)
        self.assertEqual(command.call_args_list, [mock.call("hdiutil", "detach", DEVICES[0])] * 2)


class TraceLogReader(unittest.TestCase):
    def test_waits_for_the_end_of_a_line(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "trace.log"
            line = f"12:00:00.000001  WrData[A]  D=0x10  B=0x1000  {DATA}  /x  0.000050 W kernel_task.1\n"
            path.write_text(line[:30])
            log = probe.TraceLog(path, DEVICES)
            log.poll()
            self.assertEqual(log.writes, [])
            with path.open("a") as stream:
                stream.write(line[30:])
            log.poll()
            self.assertEqual([write.size for write in log.writes], [0x1000])
            log.close()


class Barriers(unittest.TestCase):
    def test_counts_exactly_the_action_writes(self):
        fixture = Fixture(self)
        sampled = fixture.sample()
        self.assertEqual(sampled["write_bytes_by_device"][DATA]["WrData"], 3 * 8192)
        self.assertEqual(sampled["trace"]["marker_mode"], "token")
        self.assertEqual([path.name for path in fixture.mount.iterdir() if path.name.startswith(".frust-marker")], [])

    def test_a_tracer_that_starts_late_is_waited_for(self):
        sampled = Fixture(self, start=0.7).sample()
        self.assertGreater(sampled["trace"]["marker_attempts"], 1)
        self.assertEqual(sampled["write_bytes_by_device"][DATA]["WrData"], 3 * 8192)

    def test_size_matching_when_paths_are_not_printed(self):
        sampled = Fixture(self, mode="size").sample()
        self.assertEqual(sampled["trace"]["marker_mode"], "size")
        self.assertEqual(sampled["write_bytes_by_device"][DATA]["WrData"], 3 * 8192)

    def test_slow_delivery_is_waited_for_and_reported(self):
        sampled = Fixture(self, delay=0.8).sample()
        self.assertEqual(sampled["write_bytes_by_device"][DATA]["WrData"], 3 * 8192)
        self.assertGreater(sampled["trace"]["late_bytes"], 0)

    def test_late_lines_of_earlier_markers_cannot_close_a_size_matched_window(self):
        sampled = Fixture(self, mode="size", delay=0.8).sample()
        self.assertEqual(sampled["trace"]["marker_mode"], "size")
        self.assertEqual(sampled["write_bytes_by_device"][DATA]["WrData"], 3 * 8192)

    def test_a_trace_that_misses_the_action_cannot_pass(self):
        with self.assertRaisesRegex(probe.TraceError, "accounts for"):
            Fixture(self, drop=True).sample()

    def test_a_tracer_that_never_starts_cannot_pass(self):
        with self.assertRaisesRegex(probe.TraceError, "begin marker"):
            Fixture(self, start=60).sample()

    def test_tracer_messages_fail_the_window(self):
        message = "import sys, time; print('fs_usage: buffer overrun, events generated too quickly', " \
                  "file=sys.stderr, flush=True); time.sleep(30)"
        with self.assertRaisesRegex(probe.TraceError, "buffer overrun"):
            Fixture(self, command=[sys.executable, "-I", "-c", message]).sample()

    def test_a_dead_tracer_fails_the_window(self):
        with self.assertRaisesRegex(probe.TraceError, "exited"):
            Fixture(self, command=[sys.executable, "-I", "-c", "pass"]).sample()


class Soak(unittest.TestCase):
    def test_counts_the_whole_payload_per_window(self):
        fixture = Fixture(self)
        report = probe.soak(fixture.root, fixture.mount, DEVICES, [DATA], 2)
        self.assertEqual(report["failed"], 0, report)
        self.assertTrue(all(window["data_bytes"] >= probe.SOAK_PAYLOAD_BYTES for window in report["windows"]))

    def test_a_lossy_tracer_is_reported_per_window(self):
        fixture = Fixture(self, drop=True)
        report = probe.soak(fixture.root, fixture.mount, DEVICES, [DATA], 1)
        self.assertEqual(report["failed"], 1)

    def test_needs_one_calibrated_volume(self):
        fixture = Fixture(self)
        with self.assertRaises(probe.ProbeError):
            probe.soak(fixture.root, fixture.mount, DEVICES, [], 1)


if __name__ == "__main__":
    unittest.main()
