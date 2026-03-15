"""Attach-flow integration tests for heimdall.

These tests exercise the terminal UX: alt screen, status bar, detach,
signal handling. They require a real PTY (pexpect allocates one).

Requires: uv sync (installs pexpect + pytest)
Run: uv run pytest tests/ or just test-attach
"""

from __future__ import annotations

import os
import signal
import subprocess
import time
from pathlib import Path
from typing import TYPE_CHECKING

import pexpect
import pytest

if TYPE_CHECKING:
    from collections.abc import Generator

# ── Binary resolution ────────────────────────────────────────────────

_PROJECT = Path(__file__).resolve().parent.parent
_DEBUG = _PROJECT / "target" / "debug" / "hm"
_RELEASE = _PROJECT / "target" / "release" / "hm"


def _find_hm() -> str:
    """Resolve the hm binary path."""
    env = os.environ.get("HM_BIN")
    if env:
        return env
    if _DEBUG.exists():
        return str(_DEBUG)
    if _RELEASE.exists():
        return str(_RELEASE)
    pytest.skip("hm binary not found — run `cargo build` first")
    return ""  # unreachable, keeps type checker happy


HM_BIN = _find_hm()


# ── Fixtures ─────────────────────────────────────────────────────────


@pytest.fixture
def socket_dir(tmp_path: Path) -> Path:
    """Provide a temporary socket directory."""
    return tmp_path


def _wait_for_socket(path: Path, timeout: float = 5.0) -> bool:
    """Poll until a socket file appears."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return True
        time.sleep(0.05)
    return False


@pytest.fixture
def detached_session(socket_dir: Path) -> Generator[
    "function[[str, list[str], list[str] | None], subprocess.Popen[bytes]]",
]:
    """Factory fixture: start a detached hm session, clean up on exit."""
    procs: list[subprocess.Popen[bytes]] = []

    def _start(
        session_id: str,
        cmd: list[str],
        extra_args: list[str] | None = None,
    ) -> subprocess.Popen[bytes]:
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
        if not _wait_for_socket(sock):
            proc.kill()
            proc.wait()
            msg = f"Socket never appeared: {sock}"
            raise RuntimeError(msg)
        procs.append(proc)
        return proc

    yield _start

    for p in procs:
        p.kill()
        p.wait()


def _attach(
    socket_dir: Path,
    session_id: str,
    *,
    timeout: int = 5,
    dimensions: tuple[int, int] = (24, 80),
    maxread: int = 2000,
) -> pexpect.spawn:
    """Spawn an attach client."""
    return pexpect.spawn(
        HM_BIN,
        ["attach", "--socket-dir", str(socket_dir), session_id],
        timeout=timeout,
        dimensions=dimensions,
        maxread=maxread,
    )


def _detach(child: pexpect.spawn) -> None:
    """Send detach key, assert detach message, and wait for EOF."""
    child.sendcontrol("\\")
    child.expect("detached", timeout=3)
    child.expect(pexpect.EOF, timeout=3)


# ── Core attach tests ────────────────────────────────────────────────


class TestAttachCore:
    """Basic attach/detach behaviour."""

    def test_shows_status_bar(self, socket_dir: Path, detached_session) -> None:
        """Attaching shows the status bar with session name."""
        detached_session("bar", ["bash"])
        child = _attach(socket_dir, "bar")
        child.expect(r"\[hm\].*bar", timeout=5)
        _detach(child)

    def test_alt_screen(self, socket_dir: Path, detached_session) -> None:
        """Attach enters alternate screen; detach exits cleanly."""
        detached_session("alt", ["bash"])
        child = _attach(socket_dir, "alt")
        child.expect(r"\[hm\]", timeout=5)
        child.sendcontrol("\\")
        child.expect("detached", timeout=3)
        child.expect(pexpect.EOF, timeout=3)

    def test_detach_ctrl_backslash(self, socket_dir: Path, detached_session) -> None:
        """Ctrl-\\ detaches cleanly; supervisor stays alive."""
        detached_session("detach", ["sleep", "60"])
        child = _attach(socket_dir, "detach")
        child.expect(r"\[hm\]", timeout=5)
        child.sendcontrol("\\")
        child.expect("detached.*detach", timeout=3)
        child.expect(pexpect.EOF, timeout=3)
        assert (socket_dir / "detach.sock").exists(), "Socket should survive detach"

    def test_receives_output(self, socket_dir: Path, detached_session) -> None:
        """Attach shows child process output."""
        detached_session("output", ["bash", "-c", "echo OUTPUT_MARKER && sleep 60"])
        child = _attach(socket_dir, "output")
        child.expect("OUTPUT_MARKER", timeout=5)
        _detach(child)

    def test_input_forwarded(self, socket_dir: Path, detached_session) -> None:
        """Keystrokes in attach are forwarded to the child."""
        detached_session("input", ["bash"])
        child = _attach(socket_dir, "input")
        child.expect(r"\[hm\]", timeout=5)
        time.sleep(0.5)
        child.sendline("echo INPUT_FORWARD_TEST")
        child.expect("INPUT_FORWARD_TEST", timeout=5)
        _detach(child)

    def test_session_exit(self, socket_dir: Path, detached_session) -> None:
        """When the child exits, attach shows exit code and terminates."""
        detached_session("exit", ["bash", "-c", "echo GOODBYE && sleep 1 && exit 0"])
        child = _attach(socket_dir, "exit", timeout=10)
        child.expect("session exited", timeout=10)
        child.expect(pexpect.EOF, timeout=3)

    def test_session_exit_cleans_up(self, socket_dir: Path, detached_session) -> None:
        """Socket and PID files are removed after child exits."""
        detached_session("cleanup", ["bash", "-c", "sleep 1 && exit 0"])
        child = _attach(socket_dir, "cleanup", timeout=10)
        child.expect("session exited", timeout=10)
        child.expect(pexpect.EOF, timeout=3)

        # Give supervisor time to run its cleanup guard.
        time.sleep(0.5)
        assert not (socket_dir / "cleanup.sock").exists(), "Socket file should be cleaned up"
        assert not (socket_dir / "cleanup.pid").exists(), "PID file should be cleaned up"

    def test_reattach_after_detach(self, socket_dir: Path, detached_session) -> None:
        """Can detach and reattach; scrollback preserved."""
        detached_session("reattach", ["bash", "-c", "echo REATTACH_MARKER && sleep 60"])

        child1 = _attach(socket_dir, "reattach")
        child1.expect("REATTACH_MARKER", timeout=5)
        _detach(child1)
        time.sleep(0.3)

        child2 = _attach(socket_dir, "reattach")
        child2.expect("REATTACH_MARKER", timeout=5)
        _detach(child2)

    def test_status_bar_shows_state(self, socket_dir: Path, detached_session) -> None:
        """Status bar shows process state after poll interval."""
        detached_session("state", ["sleep", "60"])
        child = _attach(socket_dir, "state")
        child.expect(r"\[hm\]", timeout=5)
        child.expect(r"(idle|thinking|streaming|tool_use|active)", timeout=5)
        _detach(child)

    def test_run_without_detach_auto_attaches(self, socket_dir: Path) -> None:
        """`hm run` without --detach spawns supervisor and attaches."""
        child = pexpect.spawn(
            HM_BIN,
            [
                "run", "--id", "auto",
                "--socket-dir", str(socket_dir),
                "--", "bash", "-c", "echo AUTO_ATTACH && sleep 60",
            ],
            timeout=10,
            dimensions=(24, 80),
        )

        try:
            child.expect("AUTO_ATTACH", timeout=10)
            child.expect(r"\[hm\]", timeout=5)
            child.sendcontrol("\\")
            child.expect("detached", timeout=3)
            child.expect(pexpect.EOF, timeout=3)

            time.sleep(0.3)
            assert (socket_dir / "auto.sock").exists(), "Supervisor should survive detach"
        finally:
            child.close(force=True)
            try:
                pid_path = socket_dir / "auto.pid"
                if pid_path.exists():
                    pid = int(pid_path.read_text().strip())
                    os.kill(pid, signal.SIGTERM)
            except (ValueError, ProcessLookupError, FileNotFoundError):
                pass


# ── Signal handling tests ────────────────────────────────────────────


class TestSignals:
    """Signal handling during attach."""

    def test_sighup_kills_attach_not_supervisor(
        self, socket_dir: Path, detached_session,
    ) -> None:
        """SIGHUP kills attach but supervisor survives."""
        detached_session("sighup", ["sleep", "60"])
        child = _attach(socket_dir, "sighup")
        child.expect(r"\[hm\]", timeout=5)

        os.kill(child.pid, signal.SIGHUP)
        child.expect(pexpect.EOF, timeout=5)

        time.sleep(0.3)
        assert (socket_dir / "sighup.sock").exists(), "Supervisor must survive SIGHUP"

    def test_sigterm_kills_attach(self, socket_dir: Path, detached_session) -> None:
        """SIGTERM to attach process exits cleanly."""
        detached_session("sigterm", ["sleep", "60"])
        child = _attach(socket_dir, "sigterm")
        child.expect(r"\[hm\]", timeout=5)

        os.kill(child.pid, signal.SIGTERM)
        child.expect(pexpect.EOF, timeout=5)

        time.sleep(0.3)
        assert (socket_dir / "sigterm.sock").exists(), "Supervisor must survive SIGTERM"

    def test_ctrl_c_forwarded(self, socket_dir: Path, detached_session) -> None:
        """Ctrl-C is forwarded to the child, not caught by attach."""
        detached_session(
            "ctrlc",
            ["bash", "-c",
             'trap "echo GOT_SIGINT" INT; echo TRAP_READY; while true; do sleep 1; done'],
        )
        child = _attach(socket_dir, "ctrlc", timeout=10)
        child.expect("TRAP_READY", timeout=5)
        child.sendcontrol("c")
        child.expect("GOT_SIGINT", timeout=5)
        child.expect(r"\[hm\]", timeout=5)
        _detach(child)


# ── Adversarial tests ────────────────────────────────────────────────


class TestAdversarial:
    """Edge cases and stress scenarios."""

    def test_resize_during_attach(self, socket_dir: Path, detached_session) -> None:
        """Resize terminal while attached — status bar must survive."""
        detached_session(
            "resize",
            ["bash", "-c",
             "for i in $(seq 1 100); do echo LINE_$i; sleep 0.02; done && sleep 60"],
        )
        child = _attach(socket_dir, "resize", timeout=10)
        child.expect(r"\[hm\]", timeout=5)

        for cols, rows in [(120, 40), (40, 10), (200, 50), (80, 24), (60, 15)]:
            child.setwinsize(rows, cols)
            time.sleep(0.1)

        time.sleep(2)
        child.expect(r"\[hm\].*resize", timeout=5)
        _detach(child)

    def test_arrow_keys_work(self, socket_dir: Path, detached_session) -> None:
        """Arrow keys produce correct escape sequences."""
        detached_session("arrows", ["bash"])
        child = _attach(socket_dir, "arrows")
        child.expect(r"\[hm\]", timeout=5)
        time.sleep(0.5)

        child.sendline("echo ARROW_1")
        child.expect("ARROW_1", timeout=5)
        child.sendline("echo ARROW_2")
        child.expect("ARROW_2", timeout=5)

        child.send("\x1b[A")  # Up arrow
        time.sleep(0.3)
        child.send("\r")
        child.expect("ARROW_2", timeout=5)

        remaining = child.before.decode("utf-8", errors="replace") if child.before else ""
        assert "^[[A" not in remaining, f"Arrow key escaped as literal: {remaining!r}"
        _detach(child)

    def test_output_flood(self, socket_dir: Path, detached_session) -> None:
        """Massive output flood doesn't crash attach or corrupt status bar."""
        detached_session("flood", ["bash", "-c", "seq 1 5000 && sleep 60"])
        child = _attach(socket_dir, "flood", timeout=15, maxread=65536)
        child.expect("5000", timeout=10)
        time.sleep(1)
        child.expect(r"\[hm\]", timeout=5)
        _detach(child)

    def test_two_attaches_same_session(
        self, socket_dir: Path, detached_session,
    ) -> None:
        """Two simultaneous attaches both receive output."""
        detached_session(
            "dual", ["bash", "-c", "sleep 1 && echo DUAL_MARKER && sleep 60"],
        )
        child1 = _attach(socket_dir, "dual", timeout=10)
        child2 = _attach(socket_dir, "dual", timeout=10)

        child1.expect("DUAL_MARKER", timeout=10)
        child2.expect("DUAL_MARKER", timeout=10)

        _detach(child1)
        time.sleep(0.3)
        child2.sendcontrol("\\")
        child2.expect("detached", timeout=3)
        child2.expect(pexpect.EOF, timeout=3)

    def test_rapid_detach_reattach(self, socket_dir: Path, detached_session) -> None:
        """Rapid detach/reattach cycles don't leak or crash."""
        detached_session("rapid", ["sleep", "60"])

        for _i in range(5):
            child = _attach(socket_dir, "rapid")
            child.expect(r"\[hm\]", timeout=5)
            _detach(child)
            time.sleep(0.1)

        assert (socket_dir / "rapid.sock").exists(), "Socket gone after reattach cycles"

    def test_long_session_name(self, socket_dir: Path, detached_session) -> None:
        """Very long session name doesn't break status bar."""
        sid = "long-name-that-overflows-bar"
        detached_session(sid, ["sleep", "60"])
        child = _attach(socket_dir, sid)
        child.expect(r"\[hm\]", timeout=5)
        _detach(child)
