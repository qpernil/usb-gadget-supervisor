#!/usr/bin/env python3
"""Linux/root integration check; uses only /dev/null, no USB or I2C hardware."""
import os
from pathlib import Path
import pwd
import shutil
import signal
import subprocess
import sys
import tempfile
import time

if sys.platform != 'linux' or os.geteuid() != 0:
    sys.exit('Run on Linux as root, passing the built supervisor executable.')
binary = str(Path(sys.argv[1]).resolve())
account = pwd.getpwnam('nobody')
with tempfile.TemporaryDirectory(prefix='supervisor-device-test-') as directory:
    base = Path(directory)
    base.chmod(0o755)
    name = base.name
    state = Path('/var/lib') / name
    runtime = Path('/run') / name
    lock = Path('/run') / f'usb-gadget-supervisor-device-{name}.lock'
    worker = base / 'worker'
    worker.write_text('''#!/usr/bin/python3
import os, signal, stat, sys, time
from pathlib import Path
state = Path(os.environ['STATE_DIRECTORY'])
assert os.getuid() != 0 and os.getgid() != 0
assert os.getgroups() == []
assert stat.S_ISCHR(os.fstat(3).st_mode)
assert os.read(3, 1) == b''
assert os.write(3, b'test') == 4
assert 'NoNewPrivs:\\t1' in Path('/proc/self/status').read_text()
assert 'HOME' not in os.environ
if sys.argv[1] == 'wait':
    def stop(*_):
        (state / 'stopped').touch()
        sys.exit(0)
    signal.signal(signal.SIGTERM, stop)
    (state / 'ready').touch()
    while True: time.sleep(1)
sys.exit(int(sys.argv[1]))
''')
    worker.chmod(0o755)
    profile = base / 'profile.toml'

    def configure(argument='0'):
        profile.write_text(f'''schema = 1
mode = "device"
name = "{name}"
[worker]
command = "{worker}"
arguments = ["{argument}"]
run_as = "nobody"
state_directory = "{state}"
runtime_directory = "{runtime}"
[[resources]]
type = "character-device"
name = "target"
path = "/dev/null"
access = "read-write"
fd = 3
''')
        profile.chmod(0o600)

    def run(path=profile):
        return subprocess.run([binary, '--profile', str(path)], capture_output=True, timeout=10)

    process = None
    try:
        configure()
        result = run()
        assert result.returncode == 0, result.stderr.decode()
        assert state.stat().st_uid == account.pw_uid
        assert state.stat().st_mode & 0o777 == 0o700
        configure('7')
        assert run().returncode != 0
        configure()
        profile.chmod(0o666)
        assert b'root-owned' in run().stderr
        profile.chmod(0o600)
        os.chown(profile, account.pw_uid, account.pw_gid)
        assert b'root-owned' in run().stderr
        os.chown(profile, 0, 0)
        link = base / 'link.toml'
        link.symlink_to(profile)
        assert b'root-owned' in run(link).stderr
        for terminate_parent in (False, True):
            configure('wait')
            for marker in ('ready', 'stopped'):
                (state / marker).unlink(missing_ok=True)
            process = subprocess.Popen([binary, '--profile', str(profile)])
            deadline = time.monotonic() + 10
            while not (state / 'ready').exists():
                assert process.poll() is None, 'supervisor exited before worker readiness'
                assert time.monotonic() < deadline, 'worker readiness timed out'
                time.sleep(0.02)
            process.send_signal(signal.SIGKILL if terminate_parent else signal.SIGTERM)
            process.wait(timeout=10)
            while not (state / 'stopped').exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            assert (state / 'stopped').exists(), 'worker did not receive SIGTERM'
            process = None
        print('Device mode: FD 3, credentials, profile permissions, failure, stop, and parent death passed.')
    finally:
        if process is not None:
            process.kill()
            process.wait()
        shutil.rmtree(state, ignore_errors=True)
        shutil.rmtree(runtime, ignore_errors=True)
        lock.unlink(missing_ok=True)
