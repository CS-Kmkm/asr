"""Send a passive evaluation command after time to return focus to the EDIT."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import time
import uuid

from bench_e2e_passive import validate_command


def send_command(directory, command, delay, clock=time):
    sequence, _ = validate_command(command)
    status = json.loads((directory / 'status.json').read_text(encoding='utf-8'))
    if status.get('schema') != 'asr-passive-status/2':
        raise ValueError('not a passive-v2 controller directory')
    clock.sleep(delay)
    path = directory / 'command.json'
    if path.exists():
        previous = json.loads(path.read_text(encoding='utf-8'))
        if type(previous.get('sequence')) is not int or previous['sequence'] >= sequence:
            raise ValueError('use a larger, unused sequence')
    temporary = directory / ('.command-' + uuid.uuid4().hex + '.json')
    try:
        with temporary.open('x', encoding='utf-8') as output:
            json.dump(command, output, ensure_ascii=False)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--sequence', type=int, required=True)
    parser.add_argument('--action', choices=('arm', 'play', 'finish', 'reset_idle', 'exit'), required=True)
    parser.add_argument('--item-id')
    parser.add_argument('--confirm-idle', action='store_true')
    parser.add_argument('--confirm-recording', action='store_true')
    parser.add_argument('--delay', type=float, default=5)
    args = parser.parse_args(argv)
    if not 0 <= args.delay <= 60:
        parser.error('--delay must be between 0 and 60')
    if args.action in ('arm', 'finish', 'reset_idle') and not args.confirm_idle:
        parser.error('this action requires --confirm-idle')
    if args.action == 'play' and not args.confirm_recording:
        parser.error('play requires --confirm-recording')
    if args.action == 'arm' and not args.item_id:
        parser.error('arm requires --item-id')
    command = {'sequence': args.sequence, 'action': args.action,
               'operator_confirmed_app_idle': args.confirm_idle,
               'operator_confirmed_app_recording': args.confirm_recording}
    if args.item_id:
        command['item_id'] = args.item_id
    print(f'{args.delay:g}秒後に送信します。playの場合は評価欄へフォーカスを戻してください。')
    send_command(args.output_dir, command, args.delay)


if __name__ == '__main__':
    main()
