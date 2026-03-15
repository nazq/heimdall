# PTY Primer

A gentle introduction to pseudo-terminals for developers who use them
every day without thinking about it.

---

## What is a PTY?

When you open a terminal and type `ls`, what actually happens?

You might assume your keystrokes go straight to bash and bash prints
directly to your screen. The reality is more interesting. Between your
terminal emulator (Alacritty, kitty, Terminal.app, whatever you use) and
the shell, there is a kernel-managed device called a **pseudo-terminal**,
or PTY.

A PTY is a pair of virtual devices that simulate a hardware terminal. In
the old days, you had a physical device -- a VT100, a teletype -- wired
to a serial port. The kernel's terminal subsystem (the "TTY layer") was
built to talk to those devices. When physical terminals disappeared, the
kernel kept the same interface but made it virtual. That virtual version
is the PTY.

The key insight: a PTY is not one thing, it is a *pair*. One end is
called the **master** (or, in newer POSIX terminology, the
"multiplexor"). The other is the **slave** (or "subsidiary"). They are
connected: bytes written to one come out the other, but with the
kernel's terminal line discipline sitting in the middle, processing them.


## Master and slave

Think of it like a two-way mirror between a control room and a stage.

The **master** side is held by whatever is pretending to be the terminal
-- your terminal emulator, tmux, screen, or (in our case) heimdall. It
represents the "keyboard and screen" side of the old physical terminal.

The **slave** side is what the program (bash, vim, python) opens as its
stdin, stdout, and stderr. From the program's perspective, it looks
exactly like a real terminal device. It shows up as `/dev/pts/N` in the
filesystem.

The flow works like this:

1. You press a key. Your terminal emulator writes that byte to the
   master fd.
2. The kernel's line discipline receives it. In "cooked" mode, it might
   buffer it, handle backspace, echo it back. In "raw" mode, it passes
   it through immediately.
3. The byte appears on the slave side. Bash (or whatever is running)
   reads it from stdin.
4. Bash runs a command and writes output to stdout (the slave fd).
5. The output travels back through the line discipline to the master.
6. The terminal emulator reads from the master and renders the text on
   screen.

Every character you see in your terminal has made this round trip
through the kernel.

```
  ┌───────────────────┐         ┌──────────────────┐        ┌──────────────┐
  │ Terminal Emulator │         │     Kernel       │        │    Shell     │
  │ (alacritty, etc)  │         │  Line Discipline │        │  (bash, zsh) │
  │                   │         │                  │        │              │
  │  screen ◄─────────┼── read ─┤  master ← slave  ┼─ write─┤ stdout/err   │
  │                   │         │                  │        │              │
  │  keyboard ────────┼─ write ─┤  master → slave  ┼─ read ─┤ stdin        │
  └───────────────────┘         └──────────────────┘        └──────────────┘
                                    PTY pair
```

The master and slave are connected through the kernel's line discipline,
which handles echoing, line editing (cooked mode), and signal generation.


## Why not just pipes?

If a PTY is just a bidirectional byte stream, why not use a pair of
pipes? You could connect stdin/stdout to pipes and call it a day.

For simple programs like `cat` or `echo`, that actually works. But a
surprising number of programs depend on properties that only a real
terminal (or pseudo-terminal) provides:

**Terminal size.** Programs like vim, less, and htop need to know how
many rows and columns they have. They get this by calling the
`TIOCGWINSZ` ioctl on their stdout fd. A pipe does not have a terminal
size. A PTY does.

**Raw vs cooked mode.** When you type in bash, you can use backspace,
Ctrl-A, Ctrl-E, and other line-editing keys. That is the line
discipline doing "cooked mode" processing. When vim starts, it switches
the terminal to "raw mode" so it can handle every keystroke itself.
Pipes do not have modes.

**`isatty()` detection.** Many programs change their behavior depending
on whether stdout is a terminal. `ls` uses colors when connected to a
terminal, plain output when piped. `grep` does the same. Python
disables output buffering in interactive mode. These programs call
`isatty()`, which returns true for a PTY slave and false for a pipe.

**Job control signals.** When you press Ctrl-C, the kernel's terminal
driver sends SIGINT to the foreground process group. Ctrl-Z sends
SIGTSTP. This mechanism is tied to the controlling terminal, which must
be a TTY device, not a pipe.

**The controlling terminal.** Each session has at most one controlling
terminal. It is what allows the kernel to deliver SIGHUP when the
terminal disconnects, and it is how job control (`fg`, `bg`, `jobs`)
works. You cannot get a controlling terminal from a pipe.

In short: if you want a program to believe it is running interactively
in a real terminal, you need a PTY.


## What heimdall does with PTYs

heimdall is a session supervisor. It sits in the position that a
terminal emulator normally occupies: it holds the master end of the PTY.

The supervised process -- Claude Code, bash, or whatever command you
configure -- gets the slave end. From its perspective, nothing is
unusual. It calls `isatty()` and gets true. It queries the terminal size
and gets real dimensions. It can switch to raw mode, use colors, draw
full-screen TUIs. It has no idea it is being supervised.

This is the fundamental trick. By holding the master fd, heimdall can:

- **Read all output** the child produces, feeding it to connected
  clients through the Unix socket.
- **Write input** from any connected client, as if someone were typing
  on a keyboard.
- **Multiplex** -- multiple clients can connect to the same session via
  the Unix socket and see the same output, like a shared screen session.
- **Forward resize events** -- when a client's terminal changes size,
  heimdall calls `TIOCSWINSZ` on the master fd and sends `SIGWINCH` to
  the child, so it redraws at the correct dimensions.
- **Classify output** -- because all bytes flow through heimdall, it
  can run classifiers on the stream to detect idle states, prompts, or
  other patterns.

```
  ┌────────────┐
  │  Client A  │──┐
  └────────────┘  │    Unix
  ┌────────────┐  │   socket     ┌───────────┐          ┌─────────────────┐
  │  Client B  │──┼────────────► │ heimdall  │ master ──┤  PTY  │  slave  │──► child process
  └────────────┘  │              │ supervisor│◄── fd ───┤       │         │    (claude, bash)
  ┌────────────┐  │              └───────────┘          └─────────────────┘
  │  Client C  │──┘                  │
  └────────────┘               scrollback buffer
                               + idle classifier
```

Multiple clients share the same session. Each sees the full scrollback
on connect, then receives live output via broadcast.


## The fork-exec dance

How does heimdall actually spawn the supervised process? The sequence
is the classic Unix pattern, with PTY-specific steps mixed in:

**1. `openpty()`** -- Create the master/slave pair. This returns two
file descriptors: one for the master, one for the slave. Under the hood
this allocates a `/dev/pts/N` entry.

**2. `fork()`** -- Create a copy of the current process. After this
call, there are two processes running the same code. The return value
tells you which one you are: zero means you are the child, nonzero
means you are the parent.

**3. In the child:**

- Call `setsid()` to create a new session and become the session leader.
  This detaches from the parent's controlling terminal.
- Open the slave fd (or use the ioctl `TIOCSCTTY`) to make it the
  controlling terminal for this new session.
- Duplicate the slave fd onto stdin (fd 0), stdout (fd 1), and stderr
  (fd 2) using `dup2()`.
- Close the master fd -- the child has no business with it.
- Close the original slave fd -- it is already duplicated onto 0/1/2.
- Set any environment variables (like `DRASILL_SESSION_ID`).
- Call `exec()` to replace this process image with the target command
  (e.g., `claude`). After exec, the child *is* the target command. The
  Rust code, the fork setup, all of it is gone -- replaced by the new
  program.

**4. In the parent (heimdall):**

- Close the slave fd -- only the child needs it.
- Keep the master fd.
- Start the event loop: read from the master (child output), write to
  the master (client input), accept socket connections, and watch for
  the child process to exit.

The full sequence:

```
  openpty()
     │
     ▼
  ┌──────────────────────────────────────────────────┐
  │  Parent process (heimdall)                       │
  │  master_fd ◄──────────────────────┐              │
  │  slave_fd                         │              │
  └──────────┬─────────────────────┬──┘              │
             │ fork()              │                 │
             ▼                     │                 │
  ┌──────────────────────┐         │                 │
  │  Child (pid == 0)    │         │                 │
  │                      │         │                 │
  │  1. setsid()         │   ┌─────┴──────────────┐  │
  │  2. TIOCSCTTY        │   │ Parent (pid > 0)   │  │
  │  3. dup2(slave → 0)  │   │                    │  │
  │     dup2(slave → 1)  │   │ 1. close(slave_fd) │  │
  │     dup2(slave → 2)  │   │ 2. keep master_fd  │  │
  │  4. close(master_fd) │   │ 3. event loop:     │  │
  │  5. close(slave_fd)  │   │    read master     │  │
  │  6. set env vars     │   │    accept sockets  │  │
  │  7. exec(command)    │   │    watch SIGCHLD   │  │
  │     ─── becomes ───  │   └────────────────────┘  │
  │     claude / bash    │                           │
  └──────────────────────┘                           │
             │                                       │
             └──── bytes flow through PTY ───────────┘
```

This is not unique to heimdall. Every terminal emulator, every `sshd`
session, every `script(1)` invocation does roughly the same thing. The
PTY is one of Unix's oldest and most reliable abstractions.


## A note on terminology

POSIX has been moving away from "master/slave" terminology in favor of
"multiplexor/subsidiary" (or just "ptmx/pts"). The system calls still
use the old names (`openpty`, `forkpty`, `/dev/ptmx`), and most
documentation and man pages do too. This document uses both
interchangeably -- you will encounter both in the wild.


## Further reading

- **`man 7 pty`** -- The Linux man page for pseudo-terminals. Covers
  `posix_openpt()`, `grantpt()`, `unlockpt()`, and the `/dev/ptmx`
  interface.
- **[The TTY Demystified](https://www.linusakesson.net/programming/tty/)**
  -- Linus Akesson's excellent deep dive into the full TTY subsystem,
  including the line discipline, session management, and job control.
  The best single resource on the topic.
- **`ARCH.md`** in this repository -- heimdall's architecture document,
  which covers how the PTY integration fits into the broader supervisor
  design.
