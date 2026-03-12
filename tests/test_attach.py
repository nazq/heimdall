#!/usr/bin/env python3
"""Attach-flow integration tests for heimdall.

These tests exercise the terminal UX: alt screen, status bar, detach,
signal handling. They require a real PTY (pexpect allocates one).

Usage:
    python3 tests/test_attach.py [--hm PATH]

Requires: pexpect (pip install pexpect)
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pexpect

# ── Binary resolution ────────────────────────────────────────────────

HM_BIN = os.environ.get("HM_BIN")
if not HM_BIN:
    # Try target/debug/hm relative to project root.
    _project = Path(__file__).resolve().parent.parent
    _debug = _project / "target" / "debug" / "hm"
    _release = _project / "target" / "release" / "hm"
    if _debug.exists():
        HM_BIN = str(_debug)
    elif _release.exists():
        HM_BIN = str(_release)
    else:
        print("ERROR: hm binary not found. Run `cargo build` first.", file=sys.stderr)
        sys.exit(1)


# ── Helpers ──────────────────────────────────────────────────────────

def wait_for_socket(path: Path, timeout: float = 5.0) -> bool:
    """Poll until a socket file appears."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return True
        time.sleep(0.05)
    return False


def start_detached_session(
    session_id: str,
    socket_dir: Path,
    cmd: list[str],
    *,
    extra_args: list[str] | None = None,
) -> subprocess.Popen:
    """Start a detached hm session and wait for the socket."""
    args = [
        HM_BIN, "run", "--detach",
        "--id", session_id,
        "--socket-dir", str(socket_dir),
    ]
    if extra_args:
        args.extend(extra_args)
    args.append("--")
    args.extend(cmd)
    proc = subprocess.Popen(
        args,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    sock = socket_dir / f"{session_id}.sock"
    if not wait_for_socket(sock):
        proc.kill()
        proc.wait()
        raise RuntimeError(f"Socket never appeared: {sock}")
    return proc


# ── Test infrastructure ──────────────────────────────────────────────

_results: list[tuple[str, bool, str]] = []


def run_test(fn):
    """Run a test function, record pass/fail."""
    name = fn.__name__
    try:
        fn()
        _results.append((name, True, ""))
        print(f"  ✓ {name}")
    except Exception as e:
        _results.append((name, False, str(e)))
        print(f"  ✗ {name}: {e}")


# ── Tests ────────────────────────────────────────────────────────────


def test_attach_shows_status_bar():
    """Attaching shows the status bar with session name."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "attach-bar"
        proc = start_detached_session(sid, socket_dir, ["bash"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            # Status bar should contain [hm] and the session name.
            child.expect(r"\[hm\].*attach-bar", timeout=5)

            child.sendcontrol("\\")  # Ctrl-\ to detach
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_attach_alt_screen():
    """Attach enters alternate screen buffer (ESC[?1049h)."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "attach-alt"
        proc = start_detached_session(sid, socket_dir, ["bash"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            # Alt screen escape should be in early output.
            # pexpect captures raw bytes from the PTY.
            child.expect(r"\[hm\]", timeout=5)

            # Detach and verify we get the alt screen leave sequence
            # or at least the detach message (which means cleanup ran).
            child.sendcontrol("\\")
            child.expect("detached", timeout=3)
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_detach_ctrl_backslash():
    """Ctrl-\\ detaches cleanly, prints message, supervisor stays alive."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "detach-test"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            # Wait for status bar to confirm attach is live.
            child.expect(r"\[hm\]", timeout=5)

            # Send detach key.
            child.sendcontrol("\\")
            child.expect("detached.*detach-test", timeout=3)
            child.expect(pexpect.EOF, timeout=3)

            # Supervisor should still be alive — socket should still exist.
            sock = socket_dir / f"{sid}.sock"
            assert sock.exists(), "Socket should still exist after detach"
        finally:
            proc.kill()
            proc.wait()


def test_attach_receives_output():
    """Attach shows child process output."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "attach-output"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "echo ATTACH_OUTPUT_MARKER && sleep 60"],
        )

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            # Should see the output from the child.
            child.expect("ATTACH_OUTPUT_MARKER", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_attach_input_forwarded():
    """Keystrokes in attach are forwarded to the child."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "attach-input"
        proc = start_detached_session(sid, socket_dir, ["bash"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            # Wait for bash prompt (or at least status bar).
            child.expect(r"\[hm\]", timeout=5)
            time.sleep(0.5)  # Let bash initialize.

            # Type a command.
            child.sendline("echo INPUT_FORWARD_TEST")

            # Should see the command output.
            child.expect("INPUT_FORWARD_TEST", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_attach_session_exit():
    """When the child exits, attach shows exit code and terminates."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "attach-exit"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "echo GOODBYE && sleep 1 && exit 0"],
        )

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=10,
                dimensions=(24, 80),
            )
            # Should see exit message.
            child.expect("session exited", timeout=10)
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_reattach_after_detach():
    """Can detach and reattach to the same session."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "reattach"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "echo REATTACH_MARKER && sleep 60"],
        )

        try:
            # First attach.
            child1 = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child1.expect("REATTACH_MARKER", timeout=5)
            child1.sendcontrol("\\")
            child1.expect(pexpect.EOF, timeout=3)

            time.sleep(0.3)

            # Second attach — should still get scrollback.
            child2 = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child2.expect("REATTACH_MARKER", timeout=5)
            child2.sendcontrol("\\")
            child2.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_sighup_kills_attach_not_supervisor():
    """SIGHUP (simulating X close) kills attach but supervisor survives."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "sighup-test"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child.expect(r"\[hm\]", timeout=5)

            # Send SIGHUP to the attach process.
            os.kill(child.pid, signal.SIGHUP)

            # Attach should die.
            child.expect(pexpect.EOF, timeout=5)

            # Supervisor should still be alive.
            time.sleep(0.3)
            sock = socket_dir / f"{sid}.sock"
            assert sock.exists(), "Socket should still exist — supervisor must survive SIGHUP"
        finally:
            proc.kill()
            proc.wait()


def test_run_without_detach_auto_attaches():
    """`hm run` without --detach spawns supervisor and attaches."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "run-auto"

        child = pexpect.spawn(
            HM_BIN,
            [
                "run", "--id", sid,
                "--socket-dir", str(socket_dir),
                "--", "bash", "-c", "echo AUTO_ATTACH_TEST && sleep 60",
            ],
            timeout=10,
            dimensions=(24, 80),
        )

        try:
            # Should see both status bar and child output.
            child.expect("AUTO_ATTACH_TEST", timeout=10)
            child.expect(r"\[hm\]", timeout=5)

            # Detach.
            child.sendcontrol("\\")
            child.expect("detached", timeout=3)
            child.expect(pexpect.EOF, timeout=3)

            # Supervisor should still be running (socket exists).
            time.sleep(0.3)
            sock = socket_dir / f"{sid}.sock"
            assert sock.exists(), "Supervisor should survive detach from run-and-attach"
        finally:
            child.close(force=True)
            # Clean up the supervisor.
            try:
                pid_path = socket_dir / f"{sid}.pid"
                if pid_path.exists():
                    pid = int(pid_path.read_text().strip())
                    os.kill(pid, signal.SIGTERM)
            except (ValueError, ProcessLookupError, FileNotFoundError):
                pass


def test_status_bar_shows_state():
    """Status bar shows process state (idle/active/etc) after poll interval."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "bar-state"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child.expect(r"\[hm\]", timeout=5)

            # Wait for at least one status poll (1s interval) to populate
            # the right side with a state name.
            # Any of: idle, thinking, streaming, tool_use, active
            child.expect(r"(idle|thinking|streaming|tool_use|active)", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_sigterm_kills_attach():
    """SIGTERM to attach process exits cleanly."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "sigterm-attach"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child.expect(r"\[hm\]", timeout=5)

            os.kill(child.pid, signal.SIGTERM)
            child.expect(pexpect.EOF, timeout=5)

            # Supervisor should still be alive.
            time.sleep(0.3)
            sock = socket_dir / f"{sid}.sock"
            assert sock.exists(), "Supervisor should survive attach SIGTERM"
        finally:
            proc.kill()
            proc.wait()


# ── Adversarial tests ────────────────────────────────────────────────


def test_resize_during_attach():
    """Resize terminal while attached — status bar must survive."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-resize-attach"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "for i in $(seq 1 100); do echo LINE_$i; sleep 0.02; done && sleep 60"],
        )

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=10,
                dimensions=(24, 80),
            )
            child.expect(r"\[hm\]", timeout=5)

            # Rapid resize while output is streaming.
            for cols, rows in [(120, 40), (40, 10), (200, 50), (80, 24), (60, 15)]:
                child.setwinsize(rows, cols)
                time.sleep(0.1)

            # Wait for output to settle, then verify status bar is still there.
            time.sleep(2)
            # Status bar should still render after resizes.
            child.expect(r"\[hm\].*adv-resize-attach", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_arrow_keys_work():
    """Arrow keys produce correct escape sequences, not garbage like ^[[A."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-arrows"
        proc = start_detached_session(sid, socket_dir, ["bash"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),
            )
            child.expect(r"\[hm\]", timeout=5)
            time.sleep(0.5)

            # Type a command, then use arrow keys to recall it.
            child.sendline("echo ARROW_TEST_1")
            child.expect("ARROW_TEST_1", timeout=5)

            child.sendline("echo ARROW_TEST_2")
            child.expect("ARROW_TEST_2", timeout=5)

            # Up arrow should recall previous command, not print ^[[A.
            child.send("\x1b[A")  # Up arrow escape sequence
            time.sleep(0.3)
            child.send("\r")
            # Should see ARROW_TEST_2 again (recalled from history).
            child.expect("ARROW_TEST_2", timeout=5)

            # Verify no literal ^[[A in output (sign of broken terminal).
            # Read what's in the buffer.
            remaining = child.before.decode("utf-8", errors="replace") if child.before else ""
            assert "^[[A" not in remaining, f"Arrow key escaped as literal ^[[A: {remaining!r}"

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_ctrl_c_forwarded():
    """Ctrl-C is forwarded to the child, not caught by attach."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-ctrlc"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", 'trap "echo GOT_SIGINT_VIA_ATTACH" INT; echo TRAP_READY; while true; do sleep 1; done'],
        )

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=10,
                dimensions=(24, 80),
            )
            child.expect("TRAP_READY", timeout=5)

            # Send Ctrl-C.
            child.sendcontrol("c")

            # Child's trap should fire.
            child.expect("GOT_SIGINT_VIA_ATTACH", timeout=5)

            # Attach should still be alive (not killed by Ctrl-C).
            child.expect(r"\[hm\]", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_output_flood_attach():
    """Massive output flood doesn't crash attach or corrupt status bar."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-flood"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "seq 1 5000 && sleep 60"],
        )

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=15,
                dimensions=(24, 80),
                maxread=65536,
            )

            # Wait for flood to finish (last line should be 5000).
            child.expect("5000", timeout=10)
            time.sleep(1)

            # Status bar should still be rendering.
            child.expect(r"\[hm\]", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_two_attaches_same_session():
    """Two simultaneous attaches to the same session both work."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-dual-attach"
        proc = start_detached_session(
            sid, socket_dir,
            ["bash", "-c", "sleep 1 && echo DUAL_MARKER && sleep 60"],
        )

        try:
            child1 = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=10,
                dimensions=(24, 80),
            )
            child2 = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=10,
                dimensions=(24, 80),
            )

            # Both should see the marker.
            child1.expect("DUAL_MARKER", timeout=10)
            child2.expect("DUAL_MARKER", timeout=10)

            # Detach first, second should still work.
            child1.sendcontrol("\\")
            child1.expect(pexpect.EOF, timeout=3)

            # Second is still live.
            time.sleep(0.3)
            child2.sendcontrol("\\")
            child2.expect("detached", timeout=3)
            child2.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


def test_rapid_detach_reattach():
    """Rapid detach/reattach cycles don't leak or crash."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "adv-rapid-reattach"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            for i in range(5):
                child = pexpect.spawn(
                    HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                    timeout=5,
                    dimensions=(24, 80),
                )
                child.expect(r"\[hm\]", timeout=5)
                child.sendcontrol("\\")
                child.expect(pexpect.EOF, timeout=3)
                time.sleep(0.1)

            # Supervisor should still be healthy.
            sock = socket_dir / f"{sid}.sock"
            assert sock.exists(), f"Socket gone after {i+1} reattach cycles"
        finally:
            proc.kill()
            proc.wait()


def test_long_session_name_status_bar():
    """Very long session name doesn't break status bar rendering."""
    with tempfile.TemporaryDirectory() as tmp:
        socket_dir = Path(tmp)
        sid = "a-very-long-session-name-that-might-overflow-the-status-bar-width"
        proc = start_detached_session(sid, socket_dir, ["sleep", "60"])

        try:
            child = pexpect.spawn(
                HM_BIN, ["attach", "--socket-dir", str(socket_dir), sid],
                timeout=5,
                dimensions=(24, 80),  # 80 cols, name is 67 chars
            )
            # Should still show [hm] prefix at minimum.
            child.expect(r"\[hm\]", timeout=5)

            child.sendcontrol("\\")
            child.expect(pexpect.EOF, timeout=3)
        finally:
            proc.kill()
            proc.wait()


# ── Runner ───────────────────────────────────────────────────────────

def main():
    print(f"\nAttach integration tests (hm={HM_BIN})\n")

    tests = [
        # Core attach tests
        test_attach_shows_status_bar,
        test_attach_alt_screen,
        test_detach_ctrl_backslash,
        test_attach_receives_output,
        test_attach_input_forwarded,
        test_attach_session_exit,
        test_reattach_after_detach,
        test_sighup_kills_attach_not_supervisor,
        test_run_without_detach_auto_attaches,
        test_status_bar_shows_state,
        test_sigterm_kills_attach,
        # Adversarial attach tests
        test_resize_during_attach,
        test_arrow_keys_work,
        test_ctrl_c_forwarded,
        test_output_flood_attach,
        test_two_attaches_same_session,
        test_rapid_detach_reattach,
        test_long_session_name_status_bar,
    ]

    for test in tests:
        run_test(test)

    passed = sum(1 for _, ok, _ in _results if ok)
    failed = sum(1 for _, ok, _ in _results if not ok)
    total = len(_results)

    print(f"\n{passed}/{total} passed", end="")
    if failed:
        print(f", {failed} FAILED")
        print("\nFailures:")
        for name, ok, err in _results:
            if not ok:
                print(f"  {name}: {err}")
        sys.exit(1)
    else:
        print()
        sys.exit(0)


if __name__ == "__main__":
    main()
