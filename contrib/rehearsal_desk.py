#!/usr/bin/env python3
"""Local MCP desk for the isolated bridge16 rehearsal.

MIDI control values cannot be read back here. Locally establish a baseline
with --establish CHAIR CONTROL VALUE; this sends that value and records it.
Restore it through chair_restore. State survives stdio server restarts. Status reports
observations, not a measurement of playback latency or current synth controls.
"""
import json
import fcntl
import re
import os
import subprocess
import sys
import time

CHAIRS = {'bass': 0, 'chords': 1, 'drums': 9}
CONTROLS = {'volume': 7, 'pan': 10, 'expression': 11}
DESTINATION = os.environ.get('KJ_REHEARSAL_DESK_DESTINATION', '131:0')
UNIT = os.environ.get('KJ_REHEARSAL_DESK_UNIT', 'kaijutsu-trio-midi-live')
STATE = os.environ.get('KJ_REHEARSAL_DESK_STATE', os.path.join(
    os.environ.get('XDG_STATE_HOME', os.path.expanduser('~/.local/state')),
    'kaijutsu', 'rehearsal-desk.json'))


def scope(chair, control):
    if chair not in CHAIRS or control not in CONTROLS:
        raise ValueError('unknown chair or control')


def midi_value(value):
    if type(value) is not int or not 0 <= value <= 127:
        raise ValueError('MIDI values must be integers from 0 to 127')


class Desk:
    def __init__(self, state=STATE):
        self.state = state
        if not os.path.isabs(state):
            raise ValueError('state path must be absolute')
        if not re.fullmatch(r'\d+:\d+', DESTINATION):
            raise ValueError('destination must be an ALSA client:port')

    def identity(self):
        output = subprocess.run(['aconnect', '-l'], check=True, timeout=3,
                                capture_output=True, text=True).stdout
        client, port = DESTINATION.split(':')
        pattern = rf"(?m)^client {client}: 'FLUID Synth \((\d+)\)' \[type=user,pid=\1\]\n\s+{port} 'Synth input port \(\1:{port}\)'"
        match = re.search(pattern, output)
        if not match:
            raise ValueError('destination is not the expected FluidSynth input')
        pid = match.group(1)
        with open('/proc/' + pid + '/cmdline', 'rb') as f:
            if os.path.basename(f.read().split(b'\0')[0]) != b'fluidsynth':
                raise ValueError('FluidSynth process identity does not match')
        with open('/proc/' + pid + '/stat') as f:
            start = f.read().rsplit(')', 1)[1].split()[19]
        return pid + ':' + start

    def send(self, channel, control, value):
        data = f'{0xb0 + channel:02X} {control:02X} {value:02X}'
        subprocess.run(['aseqsend', '-p', DESTINATION, data], check=True,
                       timeout=3, capture_output=True, text=True)

    def transaction(self, chair, control, value, establish=False):
        scope(chair, control)
        midi_value(value)
        os.makedirs(os.path.dirname(self.state), mode=0o700, exist_ok=True)
        with open(self.state + '.lock', 'a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            try:
                with open(self.state) as f:
                    state = json.load(f)
            except FileNotFoundError:
                state = {}
            pid = self.identity()
            key = chair + ':' + control
            if establish:
                if key in state and state[key]['pid'] == pid:
                    raise ValueError('baseline already established; restore it first')
                state[key] = {'pid': pid, 'baseline': value}
                self.save(state)
            elif key not in state or state[key]['pid'] != pid:
                raise ValueError('no baseline established for this FluidSynth process')
            self.send(CHAIRS[chair], CONTROLS[control], value)
            return {'chair': chair, 'control': control, 'sent': value,
                    'baseline': state[key]['baseline'], 'synth_pid': pid}

    def save(self, state):
        path = self.state + '.new'
        with open(path, 'w') as f:
            os.fchmod(f.fileno(), 0o600)
            json.dump(state, f)
            f.flush()
            os.fsync(f.fileno())
        os.replace(path, self.state)

    def set_control(self, chair, control, value):
        return self.transaction(chair, control, value)

    def restore(self, chair, control):
        scope(chair, control)
        os.makedirs(os.path.dirname(self.state), mode=0o700, exist_ok=True)
        with open(self.state + '.lock', 'a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with open(self.state) as f:
                state = json.load(f)
            key = chair + ':' + control
            if key not in state or state[key]['pid'] != self.identity():
                raise ValueError('no baseline established for this FluidSynth process')
            baseline = state[key]['baseline']
            self.send(CHAIRS[chair], CONTROLS[control], baseline)
            del state[key]
            self.save(state)
            return {'chair': chair, 'control': control, 'sent': baseline}

    def status(self):
        try:
            identity = self.identity()
            destination = {'address': DESTINATION, 'identity': identity}
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            destination = {'address': DESTINATION, 'error': str(exc)}
        try:
            journal = subprocess.run(['journalctl', '--user', '-u', UNIT, '-o', 'json', '--grep', '^(playing |miss |bridge ready|ready baseline)', '-n', '24', '--no-pager'], check=True, timeout=3, capture_output=True, text=True).stdout
            timing = journal_summary(journal)
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            timing = {'error': str(exc)}
        try:
            with open(self.state) as f:
                baselines = json.load(f)
        except FileNotFoundError:
            baselines = {}
        return {'timing': timing, 'observed_unix': time.time(),
                'destination': destination, 'baselines': baselines}


def journal_summary(journal):
    launches = []
    grouped = []
    misses = []
    rows = (json.loads(line) for line in journal.splitlines())
    stamped = (row for row in rows if str(row.get('__REALTIME_TIMESTAMP', '')).isdigit())
    ordered = sorted(stamped, key=lambda row: int(row['__REALTIME_TIMESTAMP']))
    invocation = ordered[-1].get('_SYSTEMD_INVOCATION_ID') if ordered else None
    for item in ordered:
        if item.get('_SYSTEMD_INVOCATION_ID') != invocation:
            continue
        message = item.get('MESSAGE', '')
        if not isinstance(message, str):
            continue
        stamp = item.get('__REALTIME_TIMESTAMP')
        if not isinstance(stamp, str) or not stamp.isdigit():
            continue
        event = re.match(r'^(playing|miss) tick=(\d+) (.*)$', message)
        if event:
            target = grouped if event[1] == 'playing' else misses
            target.append({'tick': event[2], 'realtime_us': int(stamp), 'detail': event[3]})
        match = re.search(r'^playing (\S+) \S+ channels \[([019])\].*?\bpid (\d+)(?:\s+tick[ =](\S+))?', message)
        stamp = item.get('__REALTIME_TIMESTAMP')
        if match and isinstance(stamp, str) and stamp.isdigit():
            launches.append({'chair': {0: 'bass', 1: 'chords', 9: 'drums'}[int(match[2])],
                             'block': match[1], 'realtime_us': int(stamp),
                             'tick': match[4]})
    launches = launches[-9:]
    shared = None
    if launches and launches[-1]['tick'] is not None:
        tick = launches[-1]['tick']
        group = [x for x in launches if x['tick'] == tick]
        if len({x['chair'] for x in group}) == 3 and len(group) == 3:
            shared = {'tick': tick, 'spread_ms': (max(x['realtime_us'] for x in group) - min(x['realtime_us'] for x in group)) / 1000}
    for group in reversed(grouped):
        measurement = re.search(r'channels=\[0, 1, 9\].*? launch_spread_ms=([\d.]+)', group['detail'])
        if measurement:
            shared = {'tick': group['tick'], 'spread_ms': float(measurement[1])}
            break
    return {'latest_shared_tick': shared, 'miss_count_in_window': len(misses),
            'latest_miss_in_window': misses[-1] if misses else None, 'bridge_invocation': invocation,
            'launches': launches, 'grouped_launches': grouped[-3:], 'misses': misses[-3:],
            'note': 'launch_spread_ms measures when the bridge launched the players. Journal timestamps record log receipt. Audio onset at the speakers is unmeasured.'}


def schema(properties, required):
    return {'type': 'object', 'properties': properties, 'required': required,
            'additionalProperties': False}


chair = {'type': 'string', 'enum': list(CHAIRS)}
control = {'type': 'string', 'enum': list(CONTROLS)}
value = {'type': 'integer', 'minimum': 0, 'maximum': 127}
TOOLS = [
    {'name': 'rehearsal_status', 'description': 'Read recent bridge events and ALSA sequencer connections. Log times are not playback latency.', 'inputSchema': schema({}, [])},
    {'name': 'chair_set', 'description': 'Set a control after a baseline was established locally for this FluidSynth process.', 'inputSchema': schema({'chair': chair, 'control': control, 'value': value}, ['chair', 'control', 'value'])},
    {'name': 'chair_restore', 'description': 'Restore the locally established baseline and release this control.', 'inputSchema': schema({'chair': chair, 'control': control}, ['chair', 'control'])},
]


def dispatch(desk, method, params):
    if method == 'initialize':
        return {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}},
                'serverInfo': {'name': 'rehearsal-desk', 'version': '1'}}
    if method == 'tools/list':
        return {'tools': TOOLS}
    if method != 'tools/call':
        raise ValueError('unknown method')
    try:
        name = params['name']
        args = params.get('arguments', {})
        if name == 'rehearsal_status' and not args:
            result = desk.status()
        elif name == 'chair_set':
            result = desk.set_control(**args)
        elif name == 'chair_restore':
            result = desk.restore(**args)
        else:
            raise ValueError('unknown tool or invalid arguments')
        return {'content': [{'type': 'text', 'text': json.dumps(result)}]}
    except (ValueError, TypeError, KeyError, OSError, RuntimeError, subprocess.SubprocessError) as exc:
        return {'isError': True, 'content': [{'type': 'text', 'text': str(exc)}]}


def main():
    desk = Desk()
    if len(sys.argv) > 1:
        if len(sys.argv) != 5 or sys.argv[1] != '--establish':
            raise SystemExit('usage: rehearsal_desk.py [--establish CHAIR CONTROL BASELINE]')
        print(json.dumps(desk.transaction(sys.argv[2], sys.argv[3], int(sys.argv[4]), establish=True)))
        return
    for line in sys.stdin:
        try:
            request = json.loads(line)
            if 'id' not in request:
                continue
            try:
                result = dispatch(desk, request['method'], request.get('params', {}))
                response = {'jsonrpc': '2.0', 'id': request['id'], 'result': result}
            except (KeyError, ValueError) as exc:
                response = {'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': str(exc)}}
            print(json.dumps(response), flush=True)
        except (ValueError, TypeError):
            print(json.dumps({'jsonrpc': '2.0', 'id': None, 'error': {'code': -32700, 'message': 'invalid request'}}), flush=True)


if __name__ == '__main__':
    main()
