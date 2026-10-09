#!/usr/bin/env python3
"""Exercise ldu through a real Unix PTY; use from the project directory."""
import argparse
import codecs
import errno
import fcntl
import json
import math
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def printable(value):
    """Match the TUI's escaped display without changing filesystem names."""
    output = []
    for character in os.fsencode(value).decode("utf-8", "surrogateescape"):
        code = ord(character)
        if 0xDC80 <= code <= 0xDCFF:
            output.append(f"\\x{code - 0xDC00:02x}")
        elif character in "\\\n\r\t":
            output.append({"\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t"}[character])
        elif (code <= 0x1F or 0x7F <= code <= 0x9F or code == 0x061C
              or 0x200B <= code <= 0x200F or 0x202A <= code <= 0x202E
              or code == 0x2060 or 0x2066 <= code <= 0x2069 or code == 0xFEFF):
            output.append(f"\\u{{{code:x}}}")
        else:
            output.append(character)
    return "".join(output)


def path_in_header(screen, path):
    """Navigation expects a completed scan and a fully visible path."""
    raw_path = os.fsdecode(path)
    if not os.path.isabs(raw_path):
        raw_path = os.path.join(os.getcwd(), raw_path)
    label = screen.text(header=True)[:-8].rstrip()
    phase = re.search(r"  Scan complete(?: with errors)?$", label)
    return (screen.help_at_edge() and phase is not None
            and label[:phase.start()] == printable(raw_path))


def directory_in_header(screen, root, names):
    return any(path_in_header(screen, os.path.join(os.fsdecode(root), os.fsdecode(name)))
               for name in names)


class Screen:
    """Small VT cursor/erase interpreter for the ASCII assertions below."""
    def __init__(self, rows=28, cols=100):
        self.background = None
        self.bold = False
        self.resize(rows, cols)
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.cells = [[" "] * cols for _ in range(rows)]
        self.attributes = [[(None, False)] * cols for _ in range(rows)]
        self.y = self.x = 0

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        i = 0
        while i < len(self.pending):
            c = self.pending[i]
            if c == "\x1b":
                if i + 1 >= len(self.pending):
                    break
                if self.pending[i + 1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending[i:])
                    if not match:
                        break
                    params, _, op = match.groups()
                    nums = [int(n) if n else 0 for n in params.lstrip("?").split(";")]
                    n = nums[0] or 1
                    if op == "m":
                        self.sgr(nums)
                    elif op in ("H", "f"):
                        self.y = (nums[0] or 1) - 1
                        self.x = ((nums[1] if len(nums) > 1 else 1) or 1) - 1
                    elif op == "A":
                        self.y -= n
                    elif op == "B":
                        self.y += n
                    elif op == "C":
                        self.x += n
                    elif op == "D":
                        self.x -= n
                    elif op == "G":
                        self.x = n - 1
                    elif op == "d":
                        self.y = n - 1
                    elif op == "J" and nums[0] in (2, 3):
                        self.cells = [[" "] * self.cols for _ in range(self.rows)]
                        self.attributes = [[(self.background, self.bold)] * self.cols for _ in range(self.rows)]
                    elif op == "K":
                        if 0 <= self.y < self.rows:
                            start = 0 if nums[0] in (1, 2) else self.x
                            end = self.cols if nums[0] in (0, 2) else self.x + 1
                            for x in range(max(0, start), min(self.cols, end)):
                                self.cells[self.y][x] = " "
                                self.attributes[self.y][x] = (self.background, self.bold)
                    i += match.end()
                    continue
                if self.pending[i + 1] == "]":
                    end = re.search(r"\x07|\x1b\\", self.pending[i + 2:])
                    if not end:
                        break
                    i += 2 + end.end()
                    continue
                i += 2
                continue
            if c == "\r":
                self.x = 0
            elif c == "\n":
                self.y += 1
            elif c == "\b":
                self.x = max(0, self.x - 1)
            elif ord(c) >= 32:
                if 0 <= self.y < self.rows and 0 <= self.x < self.cols:
                    self.cells[self.y][self.x] = c
                    self.attributes[self.y][self.x] = (self.background, self.bold)
                self.x += 1
            i += 1
        self.pending = self.pending[i:]

    def sgr(self, codes):
        index = 0
        while index < len(codes):
            code = codes[index]
            if code == 0:
                self.background, self.bold = None, False
            elif code == 1:
                self.bold = True
            elif code == 22:
                self.bold = False
            elif code == 49:
                self.background = None
            elif 40 <= code <= 47:
                self.background = code - 40
            elif 100 <= code <= 107:
                self.background = code - 100 + 8
            elif code in (38, 48, 58) and index + 1 < len(codes):
                mode = codes[index + 1]
                if mode == 5 and index + 2 < len(codes):
                    if code == 48:
                        self.background = codes[index + 2]
                    index += 2
                elif mode == 2 and index + 4 < len(codes):
                    if code == 48:
                        self.background = tuple(codes[index + 2:index + 5])
                    index += 4
            index += 1

    def selected_row(self):
        # The main list highlights its selected row with DarkGray background and bold.
        rows = []
        for row in range(1, self.rows):
            attributes = self.attributes[row][:min(16, self.cols)]
            if sum(style == (8, True) for style in attributes) >= 3 and "".join(self.cells[row]).strip():
                rows.append(row)
        # A diff can briefly contain both the old and new highlighted rows.
        return rows[0] if len(rows) == 1 else None

    def text(self, header=False):
        rows = self.cells[:1] if header else self.cells
        return "\n".join("".join(row) for row in rows)

    def words(self):
        return " ".join(self.text().split())

    def help_at_edge(self):
        return bool(self.rows and self.cols >= 6 and "".join(self.cells[0][-6:]) == "? help")

    def has_modal_border(self):
        # Dialogs have a black background; literal filename glyphs do not.
        return any(character in "┌┐└┘" and self.attributes[y][x][0] == 0
                   for y, row in enumerate(self.cells) for x, character in enumerate(row))

    def has_modal_title(self, title):
        for y, row in enumerate(self.cells):
            if not any(character in "┌┐" and self.attributes[y][x][0] == 0
                       for x, character in enumerate(row)):
                continue
            line = "".join(row)
            start = line.find(title)
            while start >= 0:
                if all(style == (0, True) for style in self.attributes[y][start:start + len(title)]):
                    return True
                start = line.find(title, start + 1)
        return False

    def has_graph_brackets(self):
        # Qualification starts at 100 columns with bars enabled and counts disabled.
        # The size field occupies 13 cells, followed by the 10-cell graph.
        return any(character in "[]" for row in self.cells[1:] for character in row[13:24])

    def normal_view(self):
        # Closing frames arrive in chunks: title erasure precedes the final border erasure.
        return (self.help_at_edge() and self.rows > 1
                and bool("".join(self.cells[1]).strip())
                and not self.has_modal_border())



def check_screen_widgets():
    screen = Screen(rows=3)
    row = f"{'4 KiB':>12} {'█':<10}   report[1] corner┌name q quit Help"
    screen.feed(("\x1b[1;1Hfixture[a]  Scan complete\x1b[1;95H? help\x1b[2;1H" + row).encode())
    assert screen.normal_view() and not screen.has_graph_brackets(), "filenames were mistaken for widgets"
    assert not screen.has_modal_title("Help"), "filename was mistaken for a help title"
    screen.feed("\x1b[3;27H\x1b[48;5;8m\x1b[1mselected┌name Help\x1b[0m".encode())
    assert not screen.has_modal_border(), "selected filename was mistaken for a dialog"
    assert not screen.has_modal_title("Help"), "selected filename was mistaken for a help title"
    screen.feed(b"\x1b[2;14H[]")
    assert screen.has_graph_brackets(), "bracketed graph was not detected"
    screen.feed("\x1b[2;14H█ ".encode())
    screen.feed("\x1b[3;1H\x1b[48;5;0m┌─\x1b[1m Help \x1b[22m─┐\x1b[0m".encode())
    assert screen.has_modal_border() and not screen.normal_view(), "visible dialog border was not detected"
    assert screen.has_modal_title("Help") and not screen.has_modal_title("Notice"), "dialog title was not detected"


def check_printable_names():
    cases = [
        (b"line\n\t\r\\", "line\\n\\t\\r\\\\"),
        (b"bad\xff\xc3", "bad\\xff\\xc3"),
        (b"bad\xc3(utf", "bad\\xc3(utf"),
        ("caf\u00e9", "caf\u00e9"),
        ("\x1b\x7f\x85\u061c\u200b\u202e\u2060\u2066\ufeff",
         "\\u{1b}\\u{7f}\\u{85}\\u{61c}\\u{200b}\\u{202e}\\u{2060}\\u{2066}\\u{feff}"),
    ]
    for raw, expected in cases:
        assert printable(raw) == expected, (raw, printable(raw), expected)
        assert printable(Path(os.fsdecode(raw))) == expected, "filesystem path encoding differs"

    raw = b"Help\n\t\\\xff"
    displayed = "Help\\n\\t\\\\\\xff"
    screen = Screen(rows=3)
    root = Path("/fixture")
    screen.feed(("\x1b[1;1H/fixture/" + displayed + "  Scan complete"
                 "\x1b[1;95H? help\x1b[2;1H" + displayed + "/").encode())
    assert printable(raw) + "/" in "".join(screen.cells[1]), "escaped directory row was not recognized"
    assert directory_in_header(screen, root, [raw]), "escaped directory header was not recognized"
    assert not directory_in_header(screen, root, [b"other\nname"]), "unrelated directory header matched"
    assert screen.normal_view() and not screen.has_modal_title("Help"), "escaped filename became a dialog title"

    root = Path("/fixture/folder-root")
    for path, expected in [(root, False), (root / "folder", True),
                           (root / "folder-extra", False), (root / "folder  sibling", False)]:
        screen.resize(3, 100)
        screen.feed(("\x1b[1;1H" + printable(path) + "  Scan complete"
                     "\x1b[1;95H? help").encode())
        assert directory_in_header(screen, root, ["folder"]) == expected, "directory header matched a path prefix"
        assert path_in_header(screen, path), "exact completed path was not recognized"
        assert path_in_header(screen, root) == (path == root), "parent header identity differed"

    screen.resize(3, 100)
    screen.feed(b"\x1b[1;1H/fixture/folder-root/folder  Scan complete with errors\x1b[1;95H? help")
    assert directory_in_header(screen, root, ["folder"]), "completed error status hid directory identity"


class Session:
    def __init__(self, binary, root, threads):
        self.master, self.slave = pty.openpty()
        self.saved = termios.tcgetattr(self.slave)
        self.screen = Screen()
        self.raw = bytearray()
        self.observe = None
        self.last_keys = []
        self.started = time.perf_counter()
        self.first_frame_ms = None
        self.resize(28, 100)

        def own_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        command = [local_executable(binary)]
        if threads is not None:
            command += ["--threads", str(threads)]
        command.append(str(root))
        self.child = subprocess.Popen(
            command,
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            env={**os.environ, "TERM": "xterm-256color"},
            preexec_fn=own_terminal,
        )

    def resize(self, rows, cols):
        # TIOCSWINSZ notifies the controlling PTY's foreground process group itself.
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(rows, cols)

    def pump(self, timeout):
        ready, _, _ = select.select([self.master], [], [], timeout)
        if ready:
            try:
                data = os.read(self.master, 262144)
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise
                return
            self.raw.extend(data)
            self.screen.feed(data)
            if self.observe is not None:
                self.observe(self.screen)
            if self.first_frame_ms is None and b"Waiting to scan" in self.raw:
                self.first_frame_ms = (time.perf_counter() - self.started) * 1000

    def wait(self, predicate, description, timeout=15):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline or self.child.poll() is not None:
                self.pump(0)
                if predicate():
                    return
                attributes = termios.tcgetattr(self.slave)
                unread = struct.unpack("I", fcntl.ioctl(self.slave, termios.FIONREAD, struct.pack("I", 0)))[0]
                diagnostics = {"recent_keys": self.last_keys,
                               "termios_flags": attributes[:4],
                               "canonical": bool(attributes[3] & termios.ICANON),
                               "echo": bool(attributes[3] & termios.ECHO),
                               "unread_input_bytes": unread,
                               "pending_escape": self.screen.pending,
                               "raw_tail": bytes(self.raw[-4096:])}
                raise AssertionError(f"{description}; exit={self.child.poll()}\n{self.screen.text()}\n{diagnostics!r}")
            self.pump(min(0.1, max(0, deadline - time.monotonic())))

    def key(self, data):
        self.last_keys.append(data)
        self.last_keys = self.last_keys[-16:]
        os.write(self.master, data)

    def finish(self, key=None, sig=None):
        begin = time.perf_counter()
        if key is not None:
            self.key(key)
        if sig is not None:
            self.child.send_signal(sig)
        self.wait(lambda: self.child.poll() is not None, "process did not quit", timeout=5)
        self.pump(0)
        assert self.child.returncode == 0, (self.child.returncode, self.screen.text())
        assert termios.tcgetattr(self.slave) == self.saved, "terminal attributes were not restored"
        assert b"\x1b[?1049l" in self.raw, "alternate screen was not restored"
        return (time.perf_counter() - begin) * 1000

    def close(self):
        if self.child.poll() is None:
            # Only this harness's ldu child is terminated on assertion failures.
            self.child.kill()
            self.child.wait()
        termios.tcsetattr(self.slave, termios.TCSANOW, self.saved)
        os.close(self.master)
        os.close(self.slave)


def local_executable(binary):
    value = str(binary)
    return "./" + value if not binary.is_absolute() and len(binary.parts) == 1 else value


def measured_key(session, key, predicate, description):
    sample = {"action": description, "during_scanning": "Scanning" in session.screen.text(header=True)}
    begin = time.perf_counter()
    session.key(key)
    session.wait(predicate, description)
    sample["response_ms"] = (time.perf_counter() - begin) * 1000
    return sample


def first_row(session):
    return "".join(session.screen.cells[1]) if session.screen.rows > 1 else ""


def enter_directory(session, root, names):
    display_names = [printable(name) for name in names]
    session.key(b"\x1b[H")
    session.wait(lambda: any(name + "/" in first_row(session) for name in display_names),
                 "directory-first ordering did not become visible")
    session.key(b"\x1b[H")
    session.key(b"l")
    session.wait(lambda: directory_in_header(session.screen, root, names),
                 "directory navigation did not update the current path")



def preview_rows(session):
    rows = []
    for cells in session.screen.cells[1:]:
        line = "".join(cells)
        if "│" in line:
            rows.append(line.split("│", 1)[1])
    return rows


def check_preview_rows(session):
    # Called only for the owned fixture, whose subdirectory is named nested.
    rows = preview_rows(session)
    nested = next((row for row in rows if "nested/" in row), None)
    assert nested is not None, "hover preview omitted its known subdirectory"
    assert any(glyph in nested for glyph in "▏▎▍▌▋▊▉█"), "preview omitted the size distribution bar"
    assert not any(marker in row[1:13] for row in rows for marker in ("≥", "≤", "<=", ">=")), "preview still has approximation prefixes"
    assert "item-0000" not in "\n".join(rows), "preview included regular files"


def check_reorder_navigation(session, inputs):
    # The sole nested directory stays first in all sort modes with directories first.
    session.wait(lambda: "nested/" in first_row(session), "nested directory was not first")
    session.key(b"\x1b[H")
    session.wait(lambda: session.screen.selected_row() == 1, "Home did not select nested directory")
    for index, sort_key in enumerate((b"n", b"s", b"n", b"s", b"n", b"s")):
        session.key(b"\x1b[H")
        session.wait(lambda: session.screen.selected_row() == 1
                     and "nested/" in first_row(session), "Home did not select the first directory")
        session.key(sort_key)
        inputs.append(measured_key(session, b"j", lambda: session.screen.selected_row() == 2,
                                   f"move down during reorder {index + 1}"))
        inputs.append(measured_key(session, b"k", lambda: session.screen.selected_row() == 1,
                                   f"move up during reorder {index + 1}"))
    # Scheduling may allow a requested sort to publish between Python's key writes.
    # Deterministic Rust tests cover identity and visual position across publication.
    session.key(b"\x1b[H")
    session.wait(lambda: session.screen.selected_row() == 1
                 and "nested/" in first_row(session), "reordered Home did not select nested directory")


def check_owned_modal_geometry(session, root):
    # This directory contains only the long, space-containing fixture filename.
    fixture_file = next((root / "large" / "nested").iterdir())
    enter_directory(session, root / "large", ["nested"])
    session.wait(lambda: "geometry" in first_row(session), "geometry fixture was not listed")
    session.key(b"\x1b[H")
    session.key(b"i")
    session.wait(lambda: "Info" in session.screen.text(), "entry information did not open")
    session.resize(8, 38)
    session.wait(lambda: "Info" in session.screen.text() and "Enter / Esc: close" in session.screen.words(),
                 "small information modal lost its fixed controls")
    session.key(b"\x1b[F")
    session.wait(lambda: "Complete" in session.screen.text(), "information End did not reach details")
    session.key(b"\x1b[H")
    prefix = printable(root.absolute())[:20]
    session.wait(lambda: prefix in session.screen.text(), "information Home did not return to the path")
    for _ in range(32):
        if "END-OF-OWNED-NAME" in session.screen.text():
            break
        before = session.screen.text()
        session.key(b"j")
        session.wait(lambda: session.screen.text() != before, "information path did not scroll")
        assert "Info" in session.screen.text() and "Enter / Esc: close" in session.screen.words()
    assert "END-OF-OWNED-NAME" in session.screen.text(), "information truncated the long filename"
    session.key(b"\x1b")
    session.wait(lambda: "Info" not in session.screen.text() and session.screen.normal_view(), "information did not close")

    session.key(b"?")
    session.wait(lambda: session.screen.has_modal_title("Help"), "small help did not open")
    session.key(b"\x1b[F")
    session.wait(lambda: "counted once." in session.screen.words(), "help End did not reach the final hardlink explanation")
    assert session.screen.has_modal_title("Help") and "Enter / Esc: close" in session.screen.words()
    session.key(b"\x1b[H")
    session.wait(lambda: "Move selection" in session.screen.words(), "help Home did not return to shortcuts")
    before = session.screen.text()
    session.key(b"\x1b[6~")
    session.wait(lambda: session.screen.text() != before, "help PageDown did not scroll")
    session.key(b"\x1b[5~")
    session.wait(lambda: "Move selection" in session.screen.words(), "help PageUp did not return to shortcuts")
    session.key(b"\x1b")
    session.wait(lambda: not session.screen.has_modal_title("Help") and session.screen.normal_view(), "small help did not close")

    # The long path must scroll independently of the pinned confirmation keys.
    session.key(b"d")
    session.wait(lambda: "Delete?" in session.screen.text(), "long-path deletion confirmation missing")
    session.key(b"\x1b[F")
    session.wait(lambda: "END-OF-OWNED-NAME" in session.screen.text(), "deletion End did not reveal the full path")
    assert "y / Enter: delete" in session.screen.words() and "n / Esc: cancel" in session.screen.words()
    session.key(b"n")
    session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view(), "long-path deletion did not cancel")
    assert fixture_file.is_file(), "modal geometry check deleted its fixture"
    # Shrink an already reviewable dialog: previously visible confirmation must reset.
    session.key(b"d")
    session.wait(lambda: "Delete?" in session.screen.text(), "deletion confirmation did not reopen")
    session.resize(2, 20)
    session.wait(lambda: "Resize to review" in session.screen.words()
                 and "n / Esc: cancel" in session.screen.words(), "tiny deletion refusal was not visible")
    session.key(b"y\rn")
    try:
        session.wait(lambda: "Resize to review" not in session.screen.words() and session.screen.normal_view(), "tiny deletion dialog did not cancel")
    except AssertionError as error:
        raise AssertionError(f"{error}\nOwned geometry fixture still exists: {fixture_file.is_file()}") from None
    assert fixture_file.is_file(), "tiny-terminal confirmation removed an unreviewable target"
    assert "Deleting" not in session.screen.text() and "Deleted:" not in session.screen.text(), "tiny terminal started deletion"
    session.resize(28, 100)
    session.wait(lambda: session.screen.help_at_edge() and "geometry" in first_row(session)
                 and "/nested" in session.screen.text(header=True), "normal geometry did not restore")
    session.key(b"<")
    session.wait(lambda: "/nested" not in session.screen.text(header=True), "geometry check did not return to large")


def check_hidden_and_delete(session, root):
    directory = root / "large"
    session.key(b"t")  # Return to size order so Home selects a file, not the nested directory.
    session.key(b"\x1b[H")
    session.wait(lambda: ".hidden-test" in first_row(session) or "victim" in first_row(session),
                 "size ordering did not become visible")
    hidden_name = ".hidden-test"
    initially_visible = hidden_name in session.screen.text()
    session.key(b"e")
    session.key(b"\x1b[H")
    session.wait(lambda: (hidden_name in session.screen.text()) != initially_visible, "hidden toggle had no effect")
    session.key(b"e")
    session.key(b"\x1b[H")
    session.wait(lambda: (hidden_name in session.screen.text()) == initially_visible, "hidden toggle did not restore view")
    if initially_visible:
        session.key(b"e")
        session.key(b"\x1b[H")
        session.wait(lambda: hidden_name not in session.screen.text(), "hidden file did not hide")

    # The two visible victims are larger than every remaining individual row.
    for filename, confirm in (("victim", b"y"), ("victim-enter", b"\r")):
        session.key(b"\x1b[H")
        session.wait(lambda: first_row(session).rstrip().endswith(" " + filename),
                     "victim ordering did not become visible")
        session.key(b"d")
        session.wait(lambda: "Delete?" in session.screen.text() and filename in session.screen.text(),
                     "delete confirmation did not identify the selected victim")
        assert "y / Enter: delete" in session.screen.words() and "n / Esc: cancel" in session.screen.words()
        victim = directory / filename
        session.key(b"n")
        session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view(), "n did not cancel deletion")
        assert victim.is_file(), "cancelled deletion changed the file"
        session.key(b"d")
        session.wait(lambda: "Delete?" in session.screen.text(), "delete confirmation did not reopen")
        session.key(b"\x1b")
        session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view(), "Esc did not cancel deletion")
        assert victim.is_file(), "Escape cancellation changed the file"
        session.key(b"d")
        session.wait(lambda: "Delete?" in session.screen.text(), "delete confirmation missing")
        transcript = len(session.raw)
        session.key(confirm)
        session.wait(lambda: not victim.exists(), "confirmed asynchronous deletion did not remove the owned victim")
        session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view() and not any("".join(row).rstrip().endswith(" " + filename) for row in session.screen.cells[1:])
                     and "Scan complete" in session.screen.text(header=True), "deleted row did not leave the current view")
        assert "/large" in session.screen.text(header=True), "successful deletion changed the current directory"
        assert not any(marker in session.raw[transcript:] for marker in (b"Deleted:", b"Refreshing", b"Waiting to scan")), "successful deletion started a notice or full rescan"
        assert "Notice" not in session.screen.text(), "successful deletion opened a notice"



def has_multiple_entries(root):
    # The scanner treats a final symlink or file as a leaf; scandir only real roots.
    if root.is_symlink() or not root.is_dir():
        return False
    with os.scandir(root) as entries:
        return next(entries, None) is not None and next(entries, None) is not None

def check(binary, root, owned_fixture, max_input_ms, threads):
    metrics = []
    for quit_action in ("q", "ctrl-c", "sigterm", "missing-root"):
        session = Session(binary, root / "does-not-exist" if quit_action == "missing-root" else root, threads)
        inputs = []
        try:
            session.wait(lambda: b"Waiting to scan" in session.raw, "initial frame missing")
            assert session.raw.find(b"Waiting to scan") < session.raw.find(b"Scanning") or b"Scanning" not in session.raw
            if quit_action == "q":
                session.wait(lambda: b"Scanning" in session.raw, "scan startup frame missing")
                inputs.append(measured_key(session, b"?", lambda: session.screen.has_modal_title("Help"), "open help"))
                inputs.append(measured_key(session, b"\x1b", lambda: not session.screen.has_modal_title("Help") and session.screen.normal_view(), "close help"))
                session.wait(lambda: "Scan complete" in session.screen.text(header=True), "scan did not complete")
                # Measure actual visible selection feedback, rather than only opening overlays.
                if has_multiple_entries(root):
                    # Anchored selections may leave a valid blank tail after live resorting.
                    session.key(b"\x1b[H")
                    session.wait(lambda: session.screen.selected_row() == 1
                                 and "".join(session.screen.cells[2]).strip(), "Home did not expose two rows")
                    inputs.append(measured_key(session, b"j", lambda: session.screen.selected_row() == 2, "move selection down"))
                    inputs.append(measured_key(session, b"k", lambda: session.screen.selected_row() == 1, "move selection up"))
                # Probe input while a full visible-row reorder is in flight.
                for sort_key, label in ((b"n", "name reorder"), (b"s", "size reorder")):
                    session.key(sort_key)
                    inputs.append(measured_key(session, b"?", lambda: session.screen.has_modal_title("Help"), "help during " + label))
                    inputs.append(measured_key(session, b"\x1b", lambda: not session.screen.has_modal_title("Help") and session.screen.normal_view(), "close help during " + label))
                assert "? help" in session.screen.text(header=True)
                assert not session.screen.has_graph_brackets(), "graph bars still have brackets"
                if owned_fixture:
                    assert "q quit" not in session.screen.text(), "normal view still has a shortcut footer"
                assert not session.screen.has_modal_border(), "normal view still has panel borders"
                # Make navigation independent of file sizes and never enter symlinks.
                names = [p.name for p in root.iterdir() if p.is_dir() and not p.is_symlink()]
                assert names, "PTY corpus requires at least one real directory"
                session.key(b"t")
                session.key(b"\x1b[H")
                if owned_fixture:
                    session.wait(lambda: "large/" in first_row(session), "directory-first view did not settle")
                    session.key(b"\x1b[H")
                    session.resize(28, 140)
                    session.wait(lambda: "Subdirectories" in session.screen.text() and "nested" in session.screen.text(),
                                 "wide hovered-directory preview did not show subdirectories")
                    check_preview_rows(session)
                    session.key(b"\x1b[F")
                    session.wait(lambda: "Subdirectories" not in session.screen.text(), "file hover did not hide preview")
                    loading = set()
                    session.observe = lambda screen: loading.update(
                        marker for marker in ("Loading", "Ordering entries", "Updating", "Waiting for scan")
                        if any(marker in row for row in preview_rows(session)))
                    try:
                        session.key(b"\x1b[H")
                        session.wait(lambda: "Subdirectories" in session.screen.text()
                                     and any("nested/" in row for row in preview_rows(session)),
                                     "directory hover did not restore cached preview rows")
                    finally:
                        session.observe = None
                    assert not loading, f"hovering a cached directory flashed a placeholder: {loading}"
                    check_preview_rows(session)
                    session.resize(28, 100)
                    session.wait(lambda: session.screen.help_at_edge() and "Subdirectories" not in session.screen.text(), "narrow terminal still showed preview")
                enter_directory(session, root, names)
                if owned_fixture:
                    check_reorder_navigation(session, inputs)
                    check_owned_modal_geometry(session, root)
                    check_hidden_and_delete(session, root)
                session.wait(lambda: "Scan complete" in session.screen.text(header=True), "scan incomplete")
                # Test the ncdu '<' parent alias from a known child directory.
                if not directory_in_header(session.screen, root, names):
                    session.wait(lambda: "Scan complete" in session.screen.text(header=True), "scan incomplete")
                    enter_directory(session, root, names)
                session.key(b"<")
                session.wait(lambda: path_in_header(session.screen, root),
                             "parent navigation did not return to root")
                session.resize(5, 18)
                session.wait(lambda: session.screen.text().strip() != "", "small terminal did not redraw")
                session.resize(28, 100)
                session.wait(lambda: session.screen.help_at_edge(), "normal terminal did not redraw")
                elapsed = session.finish(key=b"q")
            elif quit_action == "ctrl-c":
                elapsed = session.finish(key=b"\x03")
            elif quit_action == "sigterm":
                elapsed = session.finish(sig=signal.SIGTERM)
            else:
                session.wait(lambda: "Scan complete with errors" in session.screen.text(header=True), "scan error not displayed")
                elapsed = session.finish(key=b"q")
            if max_input_ms is not None:
                assert all(sample["response_ms"] <= max_input_ms for sample in inputs), (max_input_ms, inputs)
            metrics.append({"case": quit_action, "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": inputs})
        finally:
            session.close()
    return metrics


def main():
    if sys.flags.optimize:
        raise SystemExit("PTY qualification requires assertions; rerun without -O or PYTHONOPTIMIZE")
    check_screen_widgets()
    check_printable_names()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ldu"))
    parser.add_argument("--root", type=Path,
                        help="reuse a synthetic corpus with ASCII-width directory paths fully visible in the header")
    parser.add_argument("--threads", type=int, help="scanner threads, 1..128; omit to qualify the application default")
    parser.add_argument("--report-latencies", action="store_true",
                        help="print latency samples; run within the benchmark's host-quiet guard")
    parser.add_argument("--max-input-ms", type=float, help="fail if observed key response exceeds this cap")
    args = parser.parse_args()
    if args.threads is not None and not 1 <= args.threads <= 128:
        parser.error("threads must be 1..128")
    if args.max_input_ms is not None and (not math.isfinite(args.max_input_ms) or args.max_input_ms <= 0):
        parser.error("max-input-ms must be a finite positive number")
    if not args.binary.is_file():
        parser.error(f"binary does not exist: {args.binary}")
    Path(".tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ldu-pty-", dir=".tmp") as work:
        root = args.root or Path(work)
        if args.root is None:
            (root / "preview-file").write_bytes(b"preview")
            for dirname, count in (("large", 300), ("small", 2)):
                directory = root / dirname
                directory.mkdir()
                for n in range(count):
                    (directory / f"item-{n:04d}").write_bytes(b"x" * 128)
            (root / "large" / "nested").mkdir()
            long_name = "geometry " + " ".join(f"long{n:02d}" for n in range(30)) + " END-OF-OWNED-NAME"
            (root / "large" / "nested" / long_name).write_bytes(b"g" * 24576)
            (root / "large" / ".hidden-test").write_bytes(b"h" * 131072)
            (root / "large" / "victim").write_bytes(b"v" * 65536)
            (root / "large" / "victim-enter").write_bytes(b"v" * 32768)
        metrics = check(args.binary, root, args.root is None, args.max_input_ms, args.threads)
    if args.report_latencies:
        print(json.dumps({"pty": metrics, "requested_threads": args.threads,
                          "uses_cli_default_threads": args.threads is None,
                          "max_first_frame_ms": max(m["first_frame_ms"] for m in metrics),
                          "max_quit_ms": max(m["quit_ms"] for m in metrics),
                          "max_input_ms": max(s["response_ms"] for m in metrics for s in m["input"]),
                          "input_cap_ms": args.max_input_ms}))
    else:
        checks = ["initial frame", "live help", "navigation", "resize", "q", "Ctrl-C", "SIGTERM",
                  "scan error", "terminal restoration"]
        if args.root is None:
            checks += ["shared preview bars", "cached hover preview", "navigation during reordering", "modal scrolling", "tiny-terminal deletion refusal", "hidden names", "incremental fixture deletion"]
        print("PTY checks passed: " + ", ".join(checks) + ".")


if __name__ == "__main__":
    main()
