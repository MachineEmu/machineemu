#!/usr/bin/env python3
"""Terminal renderer for unifi.lcm.v1 snapshots, also usable by other tools."""
import argparse
import asyncio
import json
import shutil
import sys


def safe(value):
    # JSON escaping protects terminals from untrusted firmware escape sequences.
    return json.dumps(value, ensure_ascii=True, sort_keys=True)


def render(frame, columns=100, rows=35):
    label = 'UDM-Pro' if frame.get('profile', '').startswith('udmpro-') else 'US24PRO'
    lines = [f'{label} LCM — semantic display (not pixel-exact)',
             f"Update {frame.get('sequence', '-')}  |  {frame.get('kind', 'state')}"]
    ui = frame.get('ui', {})
    lines.append('Screen: ' + safe(ui.get('screen', 'waiting')) +
                 '  State: ' + safe(ui.get('state', 'unknown')))
    if 'port.id' in ui:
        lines.append('Selected port: ' + safe(ui['port.id']))
    if frame.get('kind') == 'waiting':
        lines.append('Waiting for guest lcmd to send display state...')
    for group in ('system', 'ui'):
        lines.append(group.upper())
        for key, value in sorted(frame.get(group, {}).items()):
            lines.append(f'  {safe(key)}: {safe(value)}')
    if 'reply' in frame:
        lines.append('Last reply: ' + safe(frame['reply']))
    if 'error' in frame:
        lines.append('Error: ' + safe(frame['error']))
    if len(lines) > rows:
        lines = lines[:max(0, rows - 1)] + ['... more state available with --json']
    return '\n'.join(line[:columns] for line in lines)


async def watch(path, raw=False, once=False):
    # Broker ASCII escaping can expand one UTF-8 frame by up to six times.
    reader, writer = await asyncio.open_unix_connection(path, limit=524288)
    try:
        while line := await reader.readline():
            frame = json.loads(line)
            if raw:
                print(json.dumps(frame, ensure_ascii=True), flush=True)
            else:
                size = shutil.get_terminal_size((100, 35))
                prefix = '\x1b[2J\x1b[H' if sys.stdout.isatty() else ''
                print(prefix + render(frame, size.columns, size.lines - 1), flush=True)
            if once:
                break
    finally:
        writer.close()
        await writer.wait_closed()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('socket', help='workspace/instances/INSTANCE/lcm-events.sock')
    parser.add_argument('--json', action='store_true', help='emit full snapshots instead of terminal view')
    parser.add_argument('--once', action='store_true')
    args = parser.parse_args()
    try:
        asyncio.run(watch(args.socket, args.json, args.once))
    except KeyboardInterrupt:
        pass
    except (OSError, ValueError) as exc:
        parser.exit(1, f'lcm-view: {exc}\n')


if __name__ == '__main__':
    main()
