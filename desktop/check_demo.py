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


def window(title):
    return next((c for c in json.loads(command('hyprctl', '-j', 'clients'))
                 if c.get('class') == APP and c.get('title') == title), None)


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

            until(lambda: window('ToughFix') and window('ToughFix')['floating']
                  and window('ToughFix')['size'] == [560, 660])
            overview = window('ToughFix')
            action('demo-advanced')
            until(lambda: window('ToughFix — Advanced'))
            assert window('ToughFix — Advanced')['address'] == overview['address']
            assert window('ToughFix — Advanced')['size'] == overview['size']
            action('demo-settings')
            until(lambda: window('ToughFix — Settings'))
            assert window('ToughFix — Settings')['address'] == overview['address']
            assert window('ToughFix — Settings')['size'] == overview['size']
            action('demo-basic')
            until(lambda: window('ToughFix'))
            assert len([c for c in json.loads(command('hyprctl', '-j', 'clients'))
                        if c.get('class') == APP]) == 1
            print('PASS: Camera, Advanced and Settings tabs share one compact floating window')

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

            def tray_property(name):
                return command('gdbus', 'call', '--session', '--dest', destination,
                               '--object-path', '/' + path, '--method',
                               'org.freedesktop.DBus.Properties.Get',
                               'org.kde.StatusNotifierItem', name)

            def tray_menu():
                return command('gdbus', 'call', '--session', '--dest', destination,
                               '--object-path', '/MenuBar', '--method',
                               'com.canonical.dbusmenu.GetLayout', '0', '1', "['label']")

            idle_icon = tray_property('IconPixmap')
            action('demo-preparing')
            until(busy)
            assert tray_property('IconPixmap') != idle_icon
            assert 'Do not unplug' in tray_menu()
            action('demo-storage-ready')
            until(lambda: not busy() and tray_property('IconPixmap') == idle_icon)
            assert 'Do not unplug' not in tray_menu()
            assert 'Active' in tray_property('Status')
            action('demo-storage-error')
            until(lambda: 'NeedsAttention' in tray_property('Status'))
            assert tray_property('IconPixmap') != idle_icon
            action('demo-storage-ready')
            until(lambda: tray_property('IconPixmap') == idle_icon)
            action('demo-cached')
            until(lambda: 'eject storage' in tray_property('Title'))
            assert 'eject storage' in tray_menu()
            print('PASS: preparation completion, storage errors and mounts update the tray while upload phase stays idle')

            action('demo-camera-old')
            action('refresh-camera')
            until(busy)
            until(lambda: not busy())
            print('PASS: explicit camera refresh protects the read/remount operation')

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

    # Closing a manually opened window hides it while connected, and quits
    # without a camera. Exercise the actual GtkWindow close-request signal.
    with open('/tmp/tg-agps-desktop-demo.log', 'w') as log:
        process = subprocess.Popen([str(ROOT / 'target/debug/toughfix'),
                                    '--demo', '--state-dir',
                                    str(ROOT / 'desktop/state/demo')],
                                   cwd=ROOT, stdout=log, stderr=log)
        try:
            until(lambda: registered(process.pid) and visible())
            action('demo-close')
            until(lambda: not visible())
            assert process.poll() is None
            assert registered(process.pid)
            print('PASS: closing a connected window hides it and retains the tray')
            command('gapplication', 'launch', APP)
            until(visible)
            action('demo-disconnect')
            until(lambda: registered(process.pid) is None)
            action('demo-close')
            assert process.wait(timeout=10) == 0
            until(lambda: not visible())
            print('PASS: closing without a camera quits a manually launched app')
        finally:
            if process.poll() is None:
                action('quit')
                process.wait(timeout=15)


if __name__ == '__main__':
    main()
