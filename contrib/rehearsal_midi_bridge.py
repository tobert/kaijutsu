#!/usr/bin/env python3
"""Batch rehearsal MIDI on one delayed grid. Experimental; opt in explicitly.

The delay is presentation latency, not kernel time. The bridge drops groups
that miss their delayed deadline. It is intended for one isolated rehearsal.
"""

import argparse
import json
import os
import queue
import re
import signal
import struct
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import kjmcp


class Group:
    def __init__(self, tick, when):
        self.tick, self.when, self.entries = tick, when, []

    def add(self, block, channels, data):
        self.entries = [entry for entry in self.entries if not entry[1] & channels]
        self.entries.append((block, channels, data))


def deadline(tick, anchor_tick, anchor_time, bpm, delay):
    return anchor_time + (tick - anchor_tick) * 60 / bpm + delay


def can_play(target, now, tolerance):
    return now <= target + tolerance


def overlapping(players, channels):
    return [player for player in players if player[0] & channels]


def reanchor(anchor, bpm, new_bpm, tick, observed):
    return (tick, observed) if anchor is None or bpm != new_bpm else anchor


def vlq(data, offset):
    value = 0
    while True:
        byte = data[offset]
        offset += 1
        value = (value << 7) | (byte & 127)
        if not byte & 128:
            return value, offset


def prepare(data, bpm):
    if data[:4] != b'MThd' or len(data) < 14:
        raise ValueError('invalid MIDI header')
    size = struct.unpack('>I', data[4:8])[0]
    division = struct.unpack('>H', data[12:14])[0]
    if division & 0x8000:
        raise ValueError('SMPTE MIDI is unsupported')
    tempo = round(60000000 / bpm).to_bytes(3, 'big')
    offset, tracks, channels = 8 + size, [], set()
    while offset < len(data):
        if data[offset:offset + 4] != b'MTrk':
            raise ValueError('invalid MIDI track')
        length = struct.unpack('>I', data[offset + 4:offset + 8])[0]
        track = data[offset + 8:offset + 8 + length]
        offset += 8 + length
        out, i, running = bytearray(), 0, None
        while i < len(track):
            start = i
            _, i = vlq(track, i)
            delta = track[start:i]
            status = track[i]
            if status & 128:
                i += 1
            elif running is not None:
                status = running
            else:
                raise ValueError('invalid MIDI running status')
            if status == 0xff:
                kind = track[i]
                i += 1
                size, i = vlq(track, i)
                i += size
                out.extend(delta + b'\xff\x51\x03' + tempo if kind == 0x51 else track[start:i])
                running = None
            elif status in (0xf0, 0xf7):
                size, i = vlq(track, i)
                i += size
                out.extend(track[start:i])
                running = None
            else:
                if not 0x80 <= status <= 0xef:
                    raise ValueError('invalid MIDI status')
                running = status
                channels.add(status & 15)
                i += 1 if status & 0xe0 == 0xc0 else 2
                out.extend(track[start:i])
        tracks.append(bytes(out))
    return division, channels, tracks


def merge(entries, bpm):
    tracks, division = [], None
    for _, _, data in entries:
        div, _, parts = prepare(data, bpm)
        if division is not None and div != division:
            raise ValueError('MIDI divisions differ')
        division = div
        used = set()
        for part in parts:
            single = (b'MThd' + struct.pack('>IHHH', 6, 0, 1, div)
                + b'MTrk' + struct.pack('>I', len(part)) + part)
            _, channels, _ = prepare(single, bpm)
            if used & channels:
                raise ValueError('MIDI channels shared across tracks cannot be reset safely')
            used.update(channels)
            resets = b''.join(bytes((0, 0xb0 | channel, cc, 0))
                for channel in sorted(channels) for cc in (64, 123, 120))
            tracks.append(resets + part)
    tempo = round(60000000 / bpm).to_bytes(3, 'big')
    tracks.insert(0, b'\x00\xff\x51\x03' + tempo + b'\x00\xff\x2f\x00')
    return b'MThd' + struct.pack('>IHHH', 6, 1, len(tracks), division) + b''.join(
        b'MTrk' + struct.pack('>I', len(track)) + track for track in tracks)


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--enable', action='store_true', required=True)
    parser.add_argument('--observe', action='store_true', help='log groups without sending MIDI')
    parser.add_argument('--home', help='HOME for the MCP process')
    parser.add_argument('--mcp', required=True)
    parser.add_argument('--key', required=True)
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--track', required=True)
    parser.add_argument('--midi-port', required=True)
    parser.add_argument('--delay', type=float, default=4.0)
    args = parser.parse_args()
    if args.delay < 2:
        parser.error('--delay must be at least 2 seconds')
    env = dict(os.environ, KJMCP_QUIET='1')
    if args.home:
        env['HOME'] = args.home
    client = kjmcp.McpStdio([args.mcp, '--connect', '--host', 'localhost', '--port', str(args.port),
        '--key-file', args.key, '--context-name', 'rehearsal-admin', '--insecure', '--character', 'score-reader'], env)
    client.call('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
        'clientInfo': {'name': 'rehearsal-midi-bridge', 'version': '1'}})
    client.notify('notifications/initialized')
    time.sleep(6)
    events = queue.Queue()
    stopped = threading.Event()

    def shell(command):
        started = time.monotonic()
        raw_result = kjmcp.tool_text(client.call('tools/call', {'name': 'shell', 'arguments': {'command': command}}))
        try:
            result = json.loads(raw_result)
        except ValueError as exc:
            raise RuntimeError(f'shell {command!r} returned invalid envelope: {raw_result[:300]!r}') from exc
        elapsed = time.monotonic() - started
        if args.observe and elapsed > .5:
            print(f'slow command={command!r} elapsed_ms={elapsed * 1000:.0f}', flush=True)
        if result.get('exit_code') != 0:
            raise RuntimeError(str(result))
        return result['stdout']

    def worker():
        seen = set()
        baseline = True
        try:
            while not stopped.is_set():
                before = time.monotonic()
                raw = shell('kj transport list --details --json')
                try:
                    rows = json.loads(raw)
                except ValueError as exc:
                    raise RuntimeError(f'transport returned invalid JSON: {raw[:300]!r}') from exc
                after = time.monotonic()
                row = next((r for r in rows if r['track'] == args.track), None)
                if not row or not row['playing']:
                    events.put(('pause',))
                    baseline = True
                    stopped.wait(.2)
                    continue
                events.put(('clock', row['playhead'], (before + after) / 2, row['bpm']))
                score = row['score_context']
                listing = shell("kj block list -c '" + score + "' --tail 100")
                found = re.findall(r'^([0-9a-f]+#\d+)\s+asset/text\s+\[done\]\s+([0-9a-f]{32})\b', listing, re.M)
                if baseline:
                    seen.update(block for block, _ in found)
                    baseline = False
                for block, content_hash in found:
                    if block in seen:
                        continue
                    seen.add(block)
                    info = json.loads(shell("kj block inspect -c '" + score + "' '" + block + "' --json"))
                    with tempfile.NamedTemporaryFile() as file:
                        shell("kj cas get '" + content_hash + "' --out '" + file.name + "'")
                        data = Path(file.name).read_bytes()
                    events.put(('score', info['tick'], block, content_hash, data))
                stopped.wait(.1)
        except Exception as exc:
            events.put(('error', str(exc)))

    threading.Thread(target=worker, daemon=True).start()
    groups, anchor, bpm, players, reaping = {}, None, None, [], []
    last_clock, previous_clock = None, None
    def silence(channels):
        if args.observe or not channels:
            return
        subprocess.run(['aseqsend', '-p', args.midi_port, ' '.join(
            f'{0xb0+c:02X} 40 00 {0xb0+c:02X} 7B 00 {0xb0+c:02X} 78 00' for c in sorted(channels))],
            check=True, timeout=3)
    def clear(channels, release=True):
        cleared = set()
        for item in overlapping(players, channels):
            owned, child, path, block, start = item
            if child.poll() is None:
                child.kill()
                reaping.append(child)
            os.unlink(path)
            players.remove(item)
            cleared.update(owned)
        if release:
            silence(cleared)
    def stop(*_):
        stopped.set()
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    print('bridge ready delay_seconds=' + str(args.delay), flush=True)
    try:
        while not stopped.is_set():
            try:
                event = events.get(timeout=.01)
            except queue.Empty:
                event = None
            if event and event[0] == 'error':
                raise RuntimeError(event[1])
            if event and event[0] == 'pause':
                groups.clear()
                anchor = None
                last_clock = None
                previous_clock = None
                clear(set(range(16)))
            if event and event[0] == 'clock':
                _, tick, observed, new_bpm = event
                last_clock = observed
                if anchor is None:
                    anchor, bpm = reanchor(anchor, bpm, new_bpm, tick, observed), new_bpm
                elif bpm != new_bpm:
                    groups.clear()
                    anchor, bpm = (tick, observed), new_bpm
                    previous_clock = None
                elif previous_clock and tick == previous_clock[0] + 1:
                    edge = (previous_clock[1] + observed) / 2
                    estimate = anchor[1] + (tick - anchor[0]) * 60 / bpm
                    correction = max(-.05, min(.05, (edge - estimate) * .2))
                    anchor = (anchor[0], anchor[1] + correction)
                    for group in groups.values():
                        group.when += correction
                previous_clock = (tick, observed)
            if event and event[0] == 'score' and anchor:
                _, tick, block, content_hash, data = event
                when = deadline(tick, *anchor, bpm, args.delay)
                if not can_play(when, time.monotonic(), .05):
                    print(f'miss tick={tick} block={block} reason=late', flush=True)
                else:
                    _, channels, _ = prepare(data, bpm)
                    groups.setdefault(tick, Group(tick, when)).add(block, channels, data)
            for item in players[:]:
                channels, child, path, block, start = item
                status = child.poll()
                if status is not None:
                    players.remove(item)
                    os.unlink(path)
                    silence(channels)
                    print(f'finished block={block} pid={child.pid} status={status} channels={sorted(channels)} duration_ms={(time.monotonic()-start)*1000:.1f}', flush=True)
            reaping[:] = [child for child in reaping if child.poll() is None]
            now = time.monotonic()
            if last_clock is not None and now - last_clock > 8:
                print('miss reason=transport_timeout', flush=True)
                groups.clear()
                anchor = None
                last_clock = None
                previous_clock = None
                clear(set(range(16)))
            for tick, group in list(groups.items()):
                if group.when > now + (0 if args.observe else .2):
                    continue
                del groups[tick]
                if not can_play(group.when, now, .05):
                    print(f'miss tick={tick} reason=scheduler_late', flush=True)
                    continue
                if args.observe:
                    channels = sorted(set().union(*(entry[1] for entry in group.entries)))
                    print(f'observed tick={tick} channels={channels} blocks={",".join(x[0] for x in group.entries)} deadline_error_ms={(now-group.when)*1000:.1f}', flush=True)
                    continue
                paths = []
                for entry in group.entries:
                    with tempfile.NamedTemporaryFile(suffix='.mid', delete=False) as file:
                        file.write(merge([entry], bpm))
                        paths.append((entry, file.name))
                channels = set().union(*(entry[1] for entry in group.entries))
                stopped.wait(max(0, group.when - time.monotonic()))
                if stopped.is_set():
                    for _, path in paths:
                        os.unlink(path)
                    break
                cleanup_start = time.monotonic()
                clear(channels, release=False)
                cleanup_ms = (time.monotonic() - cleanup_start) * 1000
                if not can_play(group.when, time.monotonic(), .05):
                    for _, path in paths:
                        os.unlink(path)
                    silence(channels)
                    print(f'miss tick={tick} reason=preparation_late', flush=True)
                    continue
                launches, records = [], []
                for entry, path in paths:
                    start = time.monotonic()
                    child = subprocess.Popen(['aplaymidi', '-d', '0', '-p', args.midi_port, path])
                    launches.append(start)
                    players.append((entry[1], child, path, entry[0], start))
                    records.append((entry, child.pid, start, (time.monotonic()-start)*1000))
                for entry, pid, start, duration in records:
                    print(f'launch tick={tick} block={entry[0]} channels={sorted(entry[1])} pid={pid} monotonic={start:.6f} launch_ms={duration:.1f}', flush=True)
                spread = (max(launches) - min(launches)) * 1000
                print(f'playing tick={tick} blocks={",".join(x[0] for x in group.entries)} channels={sorted(channels)} bpm={bpm} cleanup_ms={cleanup_ms:.1f} launch_spread_ms={spread:.1f}', flush=True)
    finally:
        stopped.set()
        clear(set(range(16)))
        for child in reaping:
            child.wait(timeout=3)
        client.close()


if __name__ == '__main__':
    run()
