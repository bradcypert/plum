# Runs a program under a pseudo-terminal and reports whether it left the
# terminal as it found it. Used by `bootstrap/tty-smoke`.
#
# A separate file rather than a heredoc inside that script: the shell
# script already contains two heredocs, and nesting a third — whose body
# is itself full of quotes and backslashes — is how a delimiter collides
# with a delimiter.
import os, pty, sys, termios

prog = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.execv(prog, [prog])

before = termios.tcgetattr(fd)
out = b""
try:
    while True:
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        out += chunk
except OSError:
    pass
os.waitpid(pid, 0)
after = termios.tcgetattr(fd)

# The escape sequences must appear in LIFO order too: the program
# entered the alternate screen and then hid the cursor, so the cursor
# has to come back before the screen is given up.
text = out.decode(errors="replace")
enter_alt = text.find("\x1b[?1049h")
hide = text.find("\x1b[?25l")
show = text.find("\x1b[?25h")
leave_alt = text.find("\x1b[?1049l")
seq_ok = -1 < enter_alt < hide < show < leave_alt

print("termios" if before == after else "TERMIOS-NOT-RESTORED",
      "sequences" if seq_ok else "SEQUENCES-WRONG")
