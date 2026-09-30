#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise the real GTK/SNI lifecycle using only the device-free demo.

Needs an unlocked desktop session with a StatusNotifierWatcher and hyprctl.
Starts its own demo process; never launches the live updater or opens a camera.
"""
import json
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
APP = 'org.toughfix.Desktop.Demo'


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT)


def until(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.1)
    raise AssertionError('Timed out waiting for desktop state')


def registered(pid):
    result = command('gdbus', 'call', '--session', '--dest',
                     'org.kde.StatusNotifierWatcher', '--object-path',
                     '/StatusNotifierWatcher', '--method',
                     'org.freedesktop.DBus.Properties.Get',
                     'org.kde.StatusNotifierWatcher', 'RegisteredStatusNotifierItems')
    return next((s for s in re.findall(r"'([^']+)'", result)
                 if f'-{pid}-' in s), None)


def action(name):
    command('gapplication', 'action', APP, name)


def visible():
    return any(c.get('class') == APP
               for c in json.loads(command('hyprctl', '-j', 'clients')))


def main():
    if APP in command('gapplication', 'list-apps'):
        raise SystemExit('Quit the existing demo first; this check owns its demo process.')
    with open('/tmp/tg-agps-desktop-demo.log', 'w') as log:
        process = subprocess.Popen([str(ROOT / 'target/debug/toughfix'),
                                    '--demo', '--background', '--state-dir',
                                    str(ROOT / 'desktop/state/demo')],
                                   cwd=ROOT, stdout=log, stderr=log)
        try:
            until(lambda: registered(process.pid))
            assert not visible(), 'Background start unexpectedly opened a window'
            item = registered(process.pid)
            destination, path = item.split('/', 1)
            command('gdbus', 'call', '--session', '--dest', destination,
                    '--object-path', '/' + path, '--method',
                    'org.kde.StatusNotifierItem.Activate', '0', '0')
            until(visible)
            print('PASS: clicking tray activation opens the window')

            action('demo-hide')
            until(lambda: not visible())
            assert process.poll() is None
            print('PASS: hiding the window keeps monitoring alive')

            action('demo-disconnect')
            until(lambda: registered(process.pid) is None)
            action('demo-connect')
            until(lambda: registered(process.pid))
            print('PASS: tray disappears on disconnect and returns on connect')

            item = registered(process.pid)
            destination, path = item.split('/', 1)

            def busy():
                title = command('gdbus', 'call', '--session', '--dest', destination,
                                '--object-path', '/' + path, '--method',
                                'org.freedesktop.DBus.Properties.Get',
                                'org.kde.StatusNotifierItem', 'Title')
                return 'Do not unplug' in title

            action('upload')
            until(busy)
            status = command('gdbus', 'call', '--session', '--dest', destination,
                             '--object-path', '/' + path, '--method',
                             'org.freedesktop.DBus.Properties.Get',
                             'org.kde.StatusNotifierItem', 'Status')
            assert 'NeedsAttention' in status
            print('PASS: upload requests attention and warns against unplugging')

            layout = command('gdbus', 'call', '--session', '--dest', destination,
                             '--object-path', '/MenuBar', '--method',
                             'com.canonical.dbusmenu.GetLayout', '0', '1', "['label']")
            quit_item = re.search(r"\((\d+), \{[^}]*'label': <'Quit[^']*'>", layout)
            assert quit_item, 'Quit item missing from tray menu'
            command('gdbus', 'call', '--session', '--dest', destination,
                    '--object-path', '/MenuBar', '--method',
                    'com.canonical.dbusmenu.Event', quit_item[1], 'clicked', '<0>', '0')
            time.sleep(.3)
            assert process.poll() is None, 'Quit interrupted an active operation'
            assert process.wait(timeout=10) == 0
            until(lambda: registered(process.pid) is None)
            print('PASS: tray-menu quit waits for the operation, then removes the tray')
        finally:
            if process.poll() is None:
                action('quit')
                process.wait(timeout=15)

    # A device-triggered instance has a different disconnect lifecycle.
    with open('/tmp/tg-agps-desktop-demo.log', 'w') as log:
        process = subprocess.Popen([str(ROOT / 'target/debug/toughfix'),
                                    '--demo', '--hotplug', '--state-dir',
                                    str(ROOT / 'desktop/state/demo')],
                                   cwd=ROOT, stdout=log, stderr=log)
        try:
            until(lambda: registered(process.pid))
            duplicate = subprocess.run([str(ROOT / 'target/debug/toughfix'),
                                        '--demo', '--hotplug', '--state-dir',
                                        str(ROOT / 'desktop/state/demo')],
                                       cwd=ROOT, stdout=log, stderr=log, timeout=10)
            assert duplicate.returncode == 0
            assert not visible(), 'Duplicate camera event unexpectedly opened a window'
            print('PASS: duplicate camera activation reuses the instance silently')
            command('gapplication', 'launch', APP)
            until(visible)
            action('demo-disconnect')
            until(lambda: registered(process.pid) is None)
            time.sleep(6)
            assert process.poll() is None, 'Disconnect closed an open dashboard'
            print('PASS: an open camera-triggered dashboard survives disconnection')
            action('demo-hide')
            assert process.wait(timeout=10) == 0
            print('PASS: hidden camera-triggered instance exits after disconnection')
        finally:
            if process.poll() is None:
                action('quit')
                process.wait(timeout=15)


if __name__ == '__main__':
    main()
