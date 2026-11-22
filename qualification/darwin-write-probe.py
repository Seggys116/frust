#!/usr/bin/env python3
"""Calibrate fs_usage write accounting on an isolated macOS APFS image."""

import argparse
import collections
from contextlib import contextmanager
import fcntl
import itertools
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


TRACE_SECONDS = 120
PAYLOAD_BYTES = 8 << 20
MARKER_RETRY_SECONDS = 1.0
MARKER_LIMIT_SECONDS = 30
SETTLE_SECONDS = 1.0
QUIET_SECONDS = 2.0
QUIET_LIMIT_SECONDS = 30
POLL_SECONDS = 0.05
LEGACY_TAIL_SECONDS = 1.0
FULLFSYNC = getattr(fcntl, "F_FULLFSYNC", 51)
SOAK_FILES = 24
SOAK_WORKERS = 3
SOAK_FILE_BYTES = 16384
SOAK_BIG_BYTES = 262144
SOAK_PAYLOAD_BYTES = SOAK_WORKERS * (SOAK_FILES * SOAK_FILE_BYTES + SOAK_BIG_BYTES)
SOAK_RESIDUAL_LIMIT = 64 << 10
# Writes a build-shaped burst without fsync, then holds the CPU like a compiler would.
SOAK_WORKER = f"""
import hashlib, os, sys, time
base = sys.argv[1]
os.mkdir(base)
end = time.monotonic() + 0.8
for i in range({SOAK_FILES}):
    with open(f"{{base}}/f{{i}}", "wb") as stream:
        stream.write(os.urandom({SOAK_FILE_BYTES}))
with open(f"{{base}}/big", "wb") as stream:
    stream.write(os.urandom({SOAK_BIG_BYTES}))
while time.monotonic() < end:
    hashlib.sha256(os.urandom(65536)).digest()
"""
KINDS = ("WrData", "WrMeta", "PgOut")
WHOLE_DEVICE = re.compile(r"/dev/disk[0-9]+\Z")
DEVICE = re.compile(r"/dev/disk[0-9]+(?:s[0-9]+)*\Z")
WRITE = re.compile(r"^\s*\S+\s+(WrData|WrMeta|PgOut)(?:\[[^\]]*\])?\s")
DISK_BYTES = re.compile(r"\bB=0x([a-fA-F0-9]+)\s+(?:BC:\S+\s+)?(/dev/\S+)")
TRACE_ERROR = re.compile(r"\b(?:lost|dropped|wrapped|overflow|overrun)\b", re.IGNORECASE)
TIMESTAMP = re.compile(r"^\s*\d\d:\d\d:\d\d")
Write = collections.namedtuple("Write", "seen kind size device line")


class ProbeError(Exception):
    pass


class TraceError(ProbeError):
    """The trace of one window cannot be trusted to account for every write."""


class TraceTimeout(TraceError):
    pass


class ImageCleanupError(ProbeError):
    def __init__(self, error, root):
        super().__init__(str(error))
        self.retained_scratch = root


class CalibrationError(ProbeError):
    def __init__(self, error, result):
        super().__init__(str(error) or type(error).__name__)
        self.result = result


def command(*args):
    done = subprocess.run(args, capture_output=True, timeout=45)
    if done.returncode:
        detail = (done.stderr or done.stdout).decode(errors="replace").strip()
        raise ProbeError(f"{args[0]} {args[1]} exited {done.returncode}: {detail}")
    return done.stdout


def image_entities(result):
    return tuple(dict.fromkeys(
        item["dev-entry"] for item in result["system-entities"]
        if DEVICE.fullmatch(item.get("dev-entry", ""))
    ))


def whole_device(devices):
    return next((device for device in devices if WHOLE_DEVICE.fullmatch(device)), None)


def locate_image(image):
    listing = plistlib.loads(command("hdiutil", "info", "-plist"))
    for attached in listing["images"]:
        if Path(attached.get("image-path", "")).resolve() == image:
            return attached
    return None


def detach_image(image, device, mount):
    try:
        command("hdiutil", "detach", device)
    except ProbeError as error:
        if "resource busy" not in str(error).casefold():
            raise
        owned = locate_image(image)
        mounted = mount.is_mount()
        if not owned and not mounted:
            return
        if not owned or whole_device(image_entities(owned)) != device:
            raise ProbeError(f"cannot confirm ownership of busy image device {device}") from error
        volumes = [item["dev-entry"] for item in owned["system-entities"]
                   if item.get("mount-point") == str(mount) and DEVICE.fullmatch(item.get("dev-entry", ""))]
        if not volumes and mounted:
            info = plistlib.loads(command("diskutil", "info", "-plist", str(mount)))
            volume = info.get("DeviceNode")
            if volume in image_entities(owned) and info.get("MountPoint") == str(mount):
                volumes = [volume]
        if len(volumes) > 1 or (not volumes and mounted):
            raise ProbeError(f"cannot identify one owned volume at {mount}") from error
        if mounted:
            command("diskutil", "unmount", volumes[0])
        for attempt in range(3):
            owned = locate_image(image)
            if not owned:
                return
            if whole_device(image_entities(owned)) != device:
                raise ProbeError(f"owned image device changed after unmounting {mount}") from error
            try:
                command("hdiutil", "detach", device)
                return
            except ProbeError as retry:
                if attempt == 2 or "resource busy" not in str(retry).casefold():
                    raise
                time.sleep(0.2)


def write_record(line):
    """Return (kind, bytes, device) for a disk write line and None for other lines. Untrusted lines raise."""
    write = WRITE.match(line)
    if write:
        fields = DISK_BYTES.search(line)
        if not fields:
            raise TraceError(f"unrecognized or failed disk write: {line.strip()}")
        count, device = fields.groups()
        size = int(count, 16)
        if device == "/dev/NOTFOUND":
            # The private APFS image only issues sector-aligned block I/O.
            if size % 512:
                return None
            raise TraceError(f"disk write on a device fs_usage could not name: {line.strip()}")
        return write[1], size, device
    if (line.lstrip().startswith(("fs_usage:", "sudo:"))
            or (not TIMESTAMP.match(line) and TRACE_ERROR.search(line))):
        raise TraceError(f"disk trace rejected: {line.strip()}")
    return None


class TraceLog:
    """Reads a growing fs_usage log and keeps the writes on the image devices."""

    def __init__(self, path, devices):
        self.stream = path.open("rb")
        self.devices, self.pending, self.writes = devices, b"", []
        self.activity = time.monotonic()

    def close(self):
        self.stream.close()

    def poll(self):
        *lines, self.pending = (self.pending + self.stream.read()).split(b"\n")
        seen = time.monotonic()
        for raw in lines:
            line = raw.decode(errors="replace")
            record = write_record(line)
            if record and record[2] in self.devices:
                self.writes.append(Write(seen, *record, line))
                self.activity = seen


def available_bytes(mount):
    stats = os.statvfs(mount)
    return stats.f_bavail * stats.f_frsize


def trace_detail(path):
    with path.open("rb") as stream:
        stream.seek(max(path.stat().st_size - 1000, 0))
        return stream.read(1000).decode(errors="replace").strip()


def write_marker(path, size):
    with path.open("xb") as stream:
        stream.write(os.urandom(size))
        stream.flush()
        os.fsync(stream.fileno())
        if sys.platform == "darwin":
            fcntl.fcntl(stream.fileno(), FULLFSYNC)


def carries(write, token, size, mode):
    """Whether a data write is this marker, by file name or, when the name is not printed, by its unique size."""
    return write.kind != "WrMeta" and ((mode != "size" and token in write.line)
                                       or (mode != "token" and write.size == size))


def totals(writes, devices):
    result = {device: dict.fromkeys(KINDS, 0) for device in devices}
    for write in writes:
        result[write.device][write.kind] += write.size
    return result


def stop_tracer(tracer, name):
    if tracer.poll() is None:
        tracer.terminate()
    try:
        return tracer.wait(timeout=8)
    except subprocess.TimeoutExpired:
        tracer.kill()
        tracer.wait()
        raise ProbeError(f"fs_usage did not stop after the {name} phase")


def tracer_command():
    return ["sudo", "-n", "/usr/bin/fs_usage", "-w", "-f", "diskio", "-t", str(TRACE_SECONDS)]


class Session:
    """One fs_usage run with its log and the marker files written while it traces."""

    def __init__(self, root, mount, devices, name):
        self.mount, self.name, self.path = mount, name, root / f"{name}.log"
        self.files, self.sizes = [], itertools.count(5)
        self.sink = self.path.open("wb")
        self.tracer = subprocess.Popen(tracer_command(), stdout=self.sink, stderr=subprocess.STDOUT,
                                       stdin=subprocess.DEVNULL)
        self.log = TraceLog(self.path, devices)

    def close(self):
        try:
            return stop_tracer(self.tracer, self.name)
        finally:
            self.log.close()
            self.sink.close()
            for path in self.files:
                path.unlink(missing_ok=True)

    def wait_until(self, condition, limit, failure):
        """Poll the trace until condition() holds. A dead tracer or an expired limit is a TraceError."""
        deadline = time.monotonic() + limit
        while True:
            if self.tracer.poll() is not None:
                raise TraceError(f"fs_usage exited during the {self.name} phase: {trace_detail(self.path)}")
            self.log.poll()
            if condition():
                return
            if time.monotonic() > deadline:
                raise TraceTimeout(f"{failure}: {trace_detail(self.path)}")
            time.sleep(POLL_SECONDS)

    def barrier(self, label, mode, retry):
        """Write markers until one reaches the trace. Returns the match mode, attempts and the matching writes."""
        start, marks, found = len(self.log.writes), [], []
        limit = time.monotonic() + MARKER_LIMIT_SECONDS

        def reached():
            found[:] = [write for write in self.log.writes[start:]
                        if any(carries(write, token, size, mode) for token, size in marks)]
            return bool(found)

        while True:
            # Each marker has its own name and size, so a late line of an earlier one cannot stand in for it.
            token, size = f".frust-marker-{label}-{os.urandom(4).hex()}", next(self.sizes) * 4096
            self.files.append(self.mount / token)
            write_marker(self.files[-1], size)
            marks.append((token, size))
            try:
                self.wait_until(reached, retry or MARKER_LIMIT_SECONDS,
                                f"no {label} marker write reached the trace after {len(marks)} attempts")
            except TraceTimeout:
                if retry and time.monotonic() < limit:
                    continue
                raise
            named = any(token in write.line for write in found for token, _ in marks)
            return "token" if named else "size", len(marks), found

    def settle(self, seconds):
        """Wait for the image devices to stay silent for the given time."""
        self.wait_until(lambda: time.monotonic() - self.log.activity >= seconds, QUIET_LIMIT_SECONDS,
                        f"the image devices kept writing for {QUIET_LIMIT_SECONDS} s")


def sample(root, mount, devices, name, action):
    """Trace action() between two marker writes that must reach the log, so a missed head or tail fails."""
    os.sync()
    baseline, launched = available_bytes(mount), time.monotonic()
    session = Session(root, mount, devices, name)
    try:
        # The first markers race the tracer start. A fence marker then orders every earlier line before the window.
        mode, attempts, _ = session.barrier("begin", None, MARKER_RETRY_SECONDS)
        ready = time.monotonic() - launched
        session.barrier("fence", mode, None)
        session.settle(SETTLE_SECONDS)
        os.sync()
        first, available = len(session.log.writes), available_bytes(mount)
        action()
        os.sync()
        ended, opened = time.monotonic(), len(session.files)
        # sync(2) does not wait for the flush, so the window closes on a traced marker and then silence.
        _, _, closing = session.barrier("end", mode, None)
        marker_blocks = sum(path.stat().st_blocks * 512 for path in session.files[opened:])
        session.settle(QUIET_SECONDS)
        allocated = available - available_bytes(mount) - marker_blocks
    finally:
        status = session.close()
    if status:
        raise TraceError(f"fs_usage exited {status} in {name}: {trace_detail(session.path)}")
    names, closing = [path.name for path in session.files], {id(write) for write in closing}
    counted = [write for write in session.log.writes[first:] if id(write) not in closing
               and not (mode == "token" and write.kind != "WrMeta" and any(mark in write.line for mark in names))]
    by_device = totals(counted, devices)
    traced = sum(sum(kinds.values()) for kinds in by_device.values())
    if allocated > traced:
        raise TraceError(f"the {name} phase allocated {allocated} bytes but the trace accounts for {traced}")
    os.sync()
    return {
        "write_bytes_by_device": by_device,
        "allocated_bytes_delta": allocated,
        "trace": {
            "ready_seconds": round(ready, 3), "marker_attempts": attempts, "marker_mode": mode,
            "tail_seconds": round(max(max((write.seen for write in counted), default=ended) - ended, 0), 3),
            "late_bytes": sum(write.size for write in counted if write.seen > ended + LEGACY_TAIL_SECONDS),
            "residual_bytes": baseline - available_bytes(mount) - allocated,
        },
    }


def write_payload(path, mode, payload):
    with path.open(mode) as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())


@contextmanager
def mounted_image(work, size="512m"):
    if sys.platform != "darwin":
        raise ProbeError("calibration requires macOS")
    root = Path(tempfile.mkdtemp(prefix="frust-apfs-io-", dir=work)).resolve()
    mount = root / "volume"
    image = root / "probe.sparseimage"
    attach_attempted = False
    device = None
    try:
        mount.mkdir()
        command("hdiutil", "create", "-quiet", "-size", size, "-fs", "APFS", "-type", "SPARSE",
                "-volname", "frust-write-probe", str(image))
        attach_attempted = True
        attached = plistlib.loads(command("hdiutil", "attach", "-plist", "-nobrowse", "-mountpoint",
                                           str(mount), str(image)))
        devices = image_entities(attached)
        device = whole_device(devices)
        if not device or not any(item.get("mount-point") == str(mount) for item in attached["system-entities"]):
            raise ProbeError("hdiutil did not report the requested APFS mount and whole device")
        yield root, mount, devices
    finally:
        try:
            # Detach or preserve the scratch tree so cleanup cannot traverse a live mount.
            if attach_attempted and device is None:
                owned = locate_image(image)
                if owned:
                    device = whole_device(image_entities(owned))
                    if not device:
                        raise ProbeError(f"could not identify the whole device for attached image {image}")
            if device:
                detach_image(image, device, mount)
            if mount.is_mount():
                raise ProbeError(f"image remains mounted at {mount}")
            shutil.rmtree(root)
        except (ProbeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
            raise ImageCleanupError(error, root) from error


def calibration_result():
    return {"probe": "macos-apfs-diskimage-writes", "payload_bytes": PAYLOAD_BYTES,
            "metric": "completed writes on virtual disk-image devices",
            "phases": {}, "usable_devices": []}


def sample_calibration(root, mount, devices):
    result = calibration_result()
    result["devices"] = devices
    path, clone = mount / "payload", mount / "clone"
    if any(os.path.lexists(item) for item in (path, clone)):
        raise CalibrationError(ProbeError("calibration payload paths already exist"), result)
    try:
        first, second = os.urandom(PAYLOAD_BYTES), os.urandom(PAYLOAD_BYTES)
        phases = result["phases"]
        phases["idle"] = sample(root, mount, devices, "idle", lambda: None)
        phases["fresh"] = sample(root, mount, devices, "fresh", lambda: write_payload(path, "xb", first))
        phases["rewrite"] = sample(root, mount, devices, "rewrite", lambda: write_payload(path, "r+b", second))
        phases["clone"] = sample(root, mount, devices, "clone",
                                 lambda: command("/bin/cp", "-c", str(path), str(clone)))
        if clone.samefile(path) or clone.read_bytes() != second:
            raise ProbeError("the clone did not reproduce the rewritten bytes in a separate file")

        for candidate in devices:
            writes = {name: phase["write_bytes_by_device"][candidate] for name, phase in phases.items()}
            required = min(writes["fresh"]["WrData"], writes["rewrite"]["WrData"])
            minimum = min(sum(writes["fresh"].values()), sum(writes["rewrite"].values()))
            if required >= PAYLOAD_BYTES and all(sum(writes[name].values()) <= minimum // 4
                                                 for name in ("idle", "clone")):
                result["usable_devices"].append(candidate)
    except (ProbeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise CalibrationError(error, result) from error
    finally:
        try:
            for payload in (path, clone):
                payload.unlink(missing_ok=True)
        except OSError as error:
            raise CalibrationError(error, result) from error
    return result


def calibrate_image(root, mount, devices):
    for attempt in range(1, 4):
        try:
            result = sample_calibration(root, mount, devices)
        except CalibrationError as error:
            if not isinstance(error.__cause__, TraceError) or attempt == 3:
                raise
            print(f"APFS calibration attempt {attempt}: {error}", file=sys.stderr)
            continue
        if result["usable_devices"]:
            return result
        print(f"APFS calibration attempt {attempt}: {json.dumps(result, sort_keys=True)}", file=sys.stderr)
    raise CalibrationError("no image device recorded the full fresh and rewrite payloads with lower idle and clone writes",
                           result)


def run_workers(work):
    work.mkdir()
    workers = [subprocess.Popen([sys.executable, "-I", "-c", SOAK_WORKER, str(work / f"w{index}")],
                                stdin=subprocess.DEVNULL) for index in range(SOAK_WORKERS)]
    try:
        failed = [worker.wait(timeout=60) for worker in workers]
    finally:
        for worker in workers:
            if worker.poll() is None:
                worker.kill()
                worker.wait()
    if any(failed):
        raise ProbeError("a soak writer failed")


def soak(root, mount, devices, calibrated, count):
    """Repeat build-shaped write bursts. Each traced window must hold the whole payload and leave no marker residue."""
    if len(calibrated) != 1:
        raise ProbeError(f"expected one calibrated APFS volume, found {calibrated}")
    work, windows = mount / "soak", []
    for index in range(count):
        window = {"index": index, "problems": []}
        try:
            sampled = sample(root, mount, devices, f"soak{index}", lambda: run_workers(work))
            data = sampled["write_bytes_by_device"][calibrated[0]]["WrData"]
            window.update(data_bytes=data, trace=sampled["trace"],
                          allocated_bytes_delta=sampled["allocated_bytes_delta"])
            if data < SOAK_PAYLOAD_BYTES:
                window["problems"].append(f"the trace holds {data} data bytes of a {SOAK_PAYLOAD_BYTES} byte payload")
            if abs(sampled["trace"]["residual_bytes"]) > SOAK_RESIDUAL_LIMIT:
                window["problems"].append(f"marker files left {sampled['trace']['residual_bytes']} bytes allocated")
        except TraceError as error:
            window["problems"].append(str(error))
        finally:
            shutil.rmtree(work, ignore_errors=True)
            os.sync()
        windows.append(window)
    traces = [window["trace"] for window in windows if "trace" in window]
    return {"payload_bytes": SOAK_PAYLOAD_BYTES, "windows": windows,
            "failed": sum(bool(window["problems"]) for window in windows),
            "fixed_sleep_would_miss": {"tracer_ready_after_1s": sum(trace["ready_seconds"] > 1 for trace in traces),
                                       "flush_after_tail_1s": sum(trace["late_bytes"] > 0 for trace in traces)}}


def interrupted(_signum, _frame):
    raise ProbeError("calibration interrupted")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True, help="existing directory for the temporary image")
    parser.add_argument("--soak", type=int, default=0, help="traced write bursts to repeat after calibration")
    args = parser.parse_args()
    result = calibration_result()
    try:
        signal.signal(signal.SIGTERM, interrupted)
        with mounted_image(args.work) as (root, mount, devices):
            try:
                result = calibrate_image(root, mount, devices)
                if args.soak:
                    result["soak"] = soak(root, mount, devices, result["usable_devices"], args.soak)
                    if result["soak"]["failed"]:
                        raise ProbeError(f"{result['soak']['failed']} of {args.soak} soak windows had an incomplete trace")
            except CalibrationError as error:
                result = error.result
                raise
    except (ProbeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        result["error"] = str(error) or type(error).__name__
        result["usable_devices"].clear()
        if isinstance(error, ImageCleanupError):
            result["retained_scratch"] = str(error.retained_scratch)
    print(json.dumps(result, sort_keys=True))
    return int("error" in result)


if __name__ == "__main__":
    raise SystemExit(main())
