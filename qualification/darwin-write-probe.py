#!/usr/bin/env python3
"""Calibrate fs_usage write accounting on an isolated macOS APFS image."""

import argparse
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


TRACE_SECONDS = 6
PAYLOAD_BYTES = 8 << 20
WHOLE_DEVICE = re.compile(r"/dev/disk[0-9]+\Z")
DEVICE = re.compile(r"/dev/disk[0-9]+(?:s[0-9]+)*\Z")
WRITE = re.compile(r"^\s*\S+\s+(WrData|WrMeta|PgOut)(?:\[[^\]]*\])?\s")
DISK_BYTES = re.compile(r"\bB=0x([a-fA-F0-9]+)\s+(?:BC:\S+\s+)?(/dev/\S+)")
TRACE_ERROR = re.compile(r"\b(?:lost|dropped|wrapped|overflow|overrun)\b", re.IGNORECASE)


class ProbeError(Exception):
    pass


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
            devices = image_entities(attached)
            device = whole_device(devices)
            if not device:
                raise ProbeError(f"could not identify the whole device for attached image {image}")
            return device
    return None


def parse_bytes(path, devices):
    totals = {device: dict.fromkeys(("WrData", "WrMeta", "PgOut"), 0) for device in devices}
    with path.open(encoding="utf-8", errors="replace") as records:
        for line in records:
            write = WRITE.match(line)
            if write:
                fields = DISK_BYTES.search(line)
                if not fields:
                    raise ProbeError(f"unrecognized or failed disk write: {line.strip()}")
                count, device = fields.groups()
                if device in totals:
                    totals[device][write[1]] += int(count, 16)
            elif (line.lstrip().startswith(("fs_usage:", "sudo:"))
                  or (not re.match(r"^\s*\d\d:\d\d:\d\d", line) and TRACE_ERROR.search(line))):
                raise ProbeError(f"disk trace rejected: {line.strip()}")
    return totals


def available_bytes(mount):
    stats = os.statvfs(mount)
    return stats.f_bavail * stats.f_frsize


def trace_detail(path):
    with path.open("rb") as stream:
        stream.seek(max(path.stat().st_size - 1000, 0))
        return stream.read(1000).decode(errors="replace").strip()


def sample(root, mount, devices, name, action):
    os.sync()
    before = available_bytes(mount)
    trace_path = root / f"{name}.log"
    with trace_path.open("wb") as log:
        trace = subprocess.Popen(
            ["sudo", "-n", "/usr/bin/fs_usage", "-w", "-f", "diskio", "-t", str(TRACE_SECONDS)],
            stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
        )
        try:
            # A live tracer has no readiness signal. The full-payload checks detect missed writes.
            time.sleep(1)
            if trace.poll() is not None:
                raise ProbeError(f"fs_usage exited before the {name} phase: {trace_detail(trace_path)}")
            action()
            os.sync()
            if trace.poll() is not None:
                raise ProbeError(f"fs_usage stopped before the {name} phase was flushed")
        finally:
            try:
                status = trace.wait(timeout=TRACE_SECONDS + 3)
            except subprocess.TimeoutExpired:
                trace.terminate()
                try:
                    trace.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    trace.kill()
                    trace.wait()
                raise ProbeError(f"fs_usage did not stop after the {name} phase")
    if status:
        raise ProbeError(f"fs_usage exited {status} in {name}: {trace_detail(trace_path)}")
    return {
        "write_bytes_by_device": parse_bytes(trace_path, devices),
        "allocated_bytes_delta": before - available_bytes(mount),
    }


def write_payload(path, mode, payload):
    with path.open(mode) as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())


def calibrate(work, result):
    root = Path(tempfile.mkdtemp(prefix="frust-apfs-io-", dir=work)).resolve()
    mount = root / "volume"
    image = root / "probe.sparseimage"
    mount.mkdir()
    attach_attempted = False
    device = None
    try:
        command("hdiutil", "create", "-quiet", "-size", "512m", "-fs", "APFS", "-type", "SPARSE",
                "-volname", "frust-write-probe", str(image))
        attach_attempted = True
        attached = plistlib.loads(command("hdiutil", "attach", "-plist", "-nobrowse", "-mountpoint",
                                           str(mount), str(image)))
        devices = image_entities(attached)
        device = whole_device(devices)
        if not device or not any(item.get("mount-point") == str(mount) for item in attached["system-entities"]):
            raise ProbeError("hdiutil did not report the requested APFS mount and whole device")
        result["devices"] = devices
        path = mount / "payload"
        first, second = os.urandom(PAYLOAD_BYTES), os.urandom(PAYLOAD_BYTES)
        phases = result["phases"]
        phases["idle"] = sample(root, mount, devices, "idle", lambda: None)
        phases["fresh"] = sample(root, mount, devices, "fresh", lambda: write_payload(path, "xb", first))
        phases["rewrite"] = sample(root, mount, devices, "rewrite", lambda: write_payload(path, "r+b", second))
        clone = mount / "clone"
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
        if not result["usable_devices"]:
            raise ProbeError("no image device recorded the full fresh and rewrite payloads with lower idle and clone writes")
    finally:
        try:
            # Detach or preserve the scratch tree so cleanup cannot traverse a live mount.
            if attach_attempted:
                device = device or locate_image(image)
                if device:
                    command("hdiutil", "detach", device)
            if mount.is_mount():
                raise ProbeError(f"image remains mounted at {mount}")
            shutil.rmtree(root)
        except BaseException:
            result["retained_scratch"] = str(root)
            raise


def interrupted(_signum, _frame):
    raise ProbeError("calibration interrupted")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True, help="existing directory for the temporary image")
    args = parser.parse_args()
    result = {"probe": "macos-apfs-diskimage-writes", "payload_bytes": PAYLOAD_BYTES,
              "metric": "completed writes on virtual disk-image devices",
              "phases": {}, "usable_devices": []}
    try:
        if sys.platform != "darwin":
            raise ProbeError("calibration requires macOS")
        signal.signal(signal.SIGTERM, interrupted)
        calibrate(args.work, result)
    except (ProbeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        result["error"] = str(error) or type(error).__name__
        result["usable_devices"].clear()
    print(json.dumps(result, sort_keys=True))
    return int("error" in result)


if __name__ == "__main__":
    raise SystemExit(main())
