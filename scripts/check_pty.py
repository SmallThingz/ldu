#!/usr/bin/env python3
"""Exercise ldu through a real Unix PTY; use from the project directory."""
import argparse
import codecs
from contextlib import contextmanager
import errno
import fcntl
import json
import io
import math
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import tarfile
import termios
import time
import zipfile
import zlib


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
    label = re.sub(r"\s+(?:v select\s+)?(?:D queue\s+)?\? help\s*$", "",
                   screen.text(header=True).removeprefix("←─").removeprefix("‹ "))
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

    def main_line(self, row):
        split = self.cols * 4 // 7
        end = split if self.cols >= 96 and self.cells[row][split] == "│" else self.cols
        return "".join(self.cells[row][:end])

    def main_rows(self):
        return (self.main_line(row) for row in range(1, self.rows))

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

    for hint in ("? help", "v select  ? help", "v select  D queue  ? help"):
        path = Path("/fixture/v select ? help  Scan complete")
        screen.resize(3, 100)
        screen.feed(("\x1b[1;1H" + printable(path) + "  Scan complete"
                     + f"\x1b[1;{101 - len(hint)}H" + hint).encode())
        assert path_in_header(screen, path), "header hint was confused with a filename"
        assert not path_in_header(screen, path.parent), "header hint truncated the directory identity"


class Session:
    def __init__(self, binary, root, threads, env=None):
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
            env={**os.environ, **(env or {}), "TERM": "xterm-256color"},
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
                return False
            if not data:
                return False
            self.raw.extend(data)
            self.screen.feed(data)
            if self.observe is not None:
                self.observe(self.screen)
            if self.first_frame_ms is None and b"Waiting to scan" in self.raw:
                self.first_frame_ms = (time.perf_counter() - self.started) * 1000
            return True
        return False

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

    def finish(self, key=None, sig=None, expected_exit=0):
        begin = time.perf_counter()
        if key is not None:
            self.key(key)
        if sig is not None:
            self.child.send_signal(sig)
        self.wait(lambda: self.child.poll() is not None, "process did not quit", timeout=5)
        # PTY reads may return small chunks even after the child has exited.
        # Keep draining buffered shutdown reports before checking the transcript.
        while self.pump(0):
            pass
        assert self.child.returncode == expected_exit, (self.child.returncode, expected_exit, self.screen.text())
        assert termios.tcgetattr(self.slave) == self.saved, "terminal attributes were not restored"
        assert b"\x1b[?1049l" in self.raw, "alternate screen was not restored"
        for mode in (1000, 1002, 1003, 1006, 1015):
            enabled = f"\x1b[?{mode}h".encode()
            if enabled in self.raw:
                disabled = f"\x1b[?{mode}l".encode()
                assert self.raw.rfind(disabled) > self.raw.rfind(enabled), "mouse capture was not restored"
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
    return session.screen.main_line(1) if session.screen.rows > 1 else ""


def settled_home(session, predicate, description, multiple=True):
    session.key(b"\x1b[H")
    session.wait(lambda: session.screen.selected_row() == 1 and predicate(), description)
    # A retained row may precede a pending configuration; observe cursor changes.
    if multiple:
        session.key(b"\x1b[F")
        session.wait(lambda: session.screen.selected_row() not in (None, 1),
                     "End did not acknowledge configured order")
    session.key(b"\x1b[H")
    session.wait(lambda: session.screen.selected_row() == 1 and predicate(),
                 "Home did not return to the configured first row")


def enter_directory(session, root, names):
    display_names = [printable(name) for name in names]
    settled_home(session, lambda: any(name + "/" in first_row(session) for name in display_names),
                 "directory-first ordering did not become visible", has_multiple_entries(root))
    session.key(b"l")
    session.wait(lambda: directory_in_header(session.screen, root, names),
                 "directory navigation did not update the current path")



def preview_rows(session):
    split = session.screen.cols * 4 // 7
    return ["".join(cells[split + 1:]) for cells in session.screen.cells[1:]
            if session.screen.cols >= 96 and cells[split] == "│"]


def preview_text(session):
    return "\n".join(preview_rows(session))


def preview_fixture_ready(session):
    rows = preview_rows(session)
    return (any("Contents" in row for row in rows)
            and any(row.rstrip().endswith(" nested/") for row in rows)
            and any(row.rstrip().endswith(" victim") for row in rows))


def check_preview_rows(session):
    # Both kinds use the shared row renderer in the owned fixture's preview.
    rows = preview_rows(session)
    for name in ("nested/", "victim"):
        row = next((row for row in rows if row.rstrip().endswith(" " + name)), None)
        assert row is not None, f"hover preview omitted its known entry: {name}"
        assert any(glyph in row for glyph in "▏▎▍▌▋▊▉█"), f"preview omitted the size bar for {name}"
    assert not any(marker in row[1:13] for row in rows for marker in ("≥", "≤", "<=", ">=")), "preview still has approximation prefixes"


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
    session.wait(lambda: "Normal mode" in session.screen.words(), "help Home did not return to shortcuts")
    before = session.screen.text()
    session.key(b"\x1b[6~")
    session.wait(lambda: session.screen.text() != before, "help PageDown did not scroll")
    session.key(b"\x1b[5~")
    session.wait(lambda: "Normal mode" in session.screen.words(), "help PageUp did not return to shortcuts")
    session.key(b"\x1b")
    session.wait(lambda: not session.screen.has_modal_title("Help") and session.screen.normal_view(), "small help did not close")

    # The long path must scroll independently of the pinned confirmation keys.
    session.key(b"d")
    session.wait(lambda: "Delete?" in session.screen.text(), "long-path deletion confirmation missing")
    session.key(b"\x1b[F")
    session.wait(lambda: "END-OF-OWNED-NAME" in session.screen.text()
                 and "y / Enter: delete" in session.screen.words(), "deletion End did not reveal the full path")
    assert "y / Enter: delete" in session.screen.words() and "n / Esc: cancel" in session.screen.words()
    session.key(b"n")
    session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view(), "long-path deletion did not cancel")
    assert fixture_file.is_file(), "modal geometry check deleted its fixture"
    # Shrink an already reviewable dialog: previously visible confirmation must reset.
    session.key(b"d")
    session.wait(lambda: "Delete?" in session.screen.text()
                 and "y / Enter: delete" in session.screen.words(), "deletion confirmation did not reopen")
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
        settled_home(session, lambda: first_row(session).rstrip().endswith(" " + filename),
                     "victim ordering did not become visible")
        session.key(b"d")
        session.wait(lambda: "Delete?" in session.screen.text() and filename in session.screen.text()
                     and "y / Enter: delete" in session.screen.words(),
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
        session.wait(lambda: "Delete?" in session.screen.text()
                     and "y / Enter: delete" in session.screen.words(), "delete confirmation missing")
        transcript = len(session.raw)
        session.key(confirm)
        session.wait(lambda: not victim.exists(), "confirmed asynchronous deletion did not remove the owned victim")
        session.wait(lambda: "Delete?" not in session.screen.text() and session.screen.normal_view() and not any(row.rstrip().endswith(" " + filename) for row in session.screen.main_rows())
                     and "Scan complete" in session.screen.text(header=True), "deleted row did not leave the current view")
        assert "/large" in session.screen.text(header=True), "successful deletion changed the current directory"
        assert not any(marker in session.raw[transcript:] for marker in (b"Deleted:", b"Refreshing", b"Waiting to scan")), "successful deletion started a notice or full rescan"
        assert "Notice" not in session.screen.text(), "successful deletion opened a notice"



def fixture_selection(session, name, directory=False):
    """Select one fully visible owned-fixture row without relying on its sort."""
    suffix = " " + printable(name) + ("/" if directory else "")
    # An already-selected first row does not acknowledge Home or a pending
    # reorder/resize frame. Use the existing End/Home roundtrip before reading
    # row positions, so a partially received list cannot choose the key count.
    session.key(b"\x1b[H")
    visible = lambda: any(line.rstrip().endswith(suffix)
                         for line in session.screen.main_rows())
    session.wait(lambda: session.screen.selected_row() == 1 and visible(),
                 f"owned fixture row did not become visible: {name}")
    multiple = any(line.strip() for line in list(session.screen.main_rows())[1:])
    settled_home(session, visible, f"owned fixture row did not settle: {name}", multiple)
    row = next((index for index, line in enumerate(session.screen.main_rows(), 1)
                if line.rstrip().endswith(suffix)), None)
    assert row is not None, f"owned fixture row missing: {name}\n{session.screen.text()}"
    for _ in range(row - 1):
        session.key(b"j")
    session.wait(lambda: session.screen.selected_row() == row
                 and session.screen.main_line(row).rstrip().endswith(suffix),
                 f"fixture selection did not reach {name}")


def deletion_marks(session, names):
    return {name: "×" in row for row in session.screen.main_rows()
            for name in names if row.rstrip().endswith(" " + name)}


def fixture_directory(session, path):
    # These fixture paths contain no spaces and are fully visible. Queue status
    # replaces Scan complete in the header while paused jobs remain queued.
    return (session.screen.help_at_edge()
            and session.screen.text(header=True).removeprefix("←─").removeprefix("‹ ").startswith(printable(path.absolute()) + "  "))


def queue_rows(session):
    rows = []
    if not session.screen.has_modal_title("Deletion queue"):
        return rows
    for index, cells in enumerate(session.screen.cells):
        line = "".join(cells)
        start, end = line.find("│"), line.rfind("│")
        if start < 0 or end <= start:
            continue
        inner = line[start + 1:end].strip()
        match = re.fullmatch(r"(queued|active|cancel)\s+(.+)", inner)
        if match is not None:
            selected = any(style == (8, True)
                           for style in session.screen.attributes[index][start + 1:end])
            rows.append((match[1], match[2], selected))
    return rows


def queue_paths(session):
    return [path for _, path, _ in queue_rows(session)]


def queue_selected(session):
    selected = [path for _, path, highlighted in queue_rows(session) if highlighted]
    return selected[0] if len(selected) == 1 else None


def open_queue(session, expected):
    session.key(b"D")
    session.wait(lambda: session.screen.has_modal_title("Deletion queue")
                 and queue_paths(session) == expected
                 and "Updating queue" not in session.screen.words(),
                 f"deletion queue did not display {expected}")


def close_queue(session):
    session.key(b"\x1b")
    session.wait(lambda: session.screen.normal_view(),
                 "deletion queue did not close")


def enqueue_fixture(session, name, directory=False):
    fixture_selection(session, name, directory)
    session.key(b"d")
    session.wait(lambda: session.screen.has_modal_title("Delete?")
                 and printable(name) in session.screen.text()
                 and "y / Enter: delete" in session.screen.words(),
                 f"deletion confirmation did not identify {name}")
    session.key(b"y")
    session.wait(lambda: session.screen.normal_view(),
                 f"confirmed {name} did not return to browsing")


def pause_empty_queue(session):
    open_queue(session, [])
    session.key(b" ")
    session.wait(lambda: "Pending paused" in session.screen.words(),
                 "Space did not pause an empty deletion queue")
    close_queue(session)


def check_owned_queue(binary, work, threads):
    root = work / "queue"
    root.mkdir()
    for name in ("alpha", "beta", "gamma"):
        (root / name).write_bytes(b"q" * 4096)
    (root / "branch").mkdir()
    (root / "branch" / "leaf").write_bytes(b"l" * 4096)
    (root / "branch" / "other").write_bytes(b"o" * 4096)
    session = Session(binary, root, threads)
    try:
        # Keep the full nested path visible beside queue status and shortcuts.
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root), "queue fixture scan did not complete")
        pause_empty_queue(session)
        for name in ("alpha", "beta", "gamma", "alpha"):
            enqueue_fixture(session, name)
        session.wait(lambda: deletion_marks(session, ("alpha", "beta", "gamma"))
                     == {"alpha": True, "beta": True, "gamma": True},
                     "paused queued targets did not show deletion markers")
        open_queue(session, ["alpha", "beta", "gamma"])
        session.key(b"\x1b[H")
        session.wait(lambda: queue_selected(session) == "alpha", "queue Home did not select alpha")
        for key, order, selected in (
                (b"j", ["alpha", "beta", "gamma"], "beta"),
                (b"J", ["alpha", "gamma", "beta"], "beta"),
                (b"K", ["alpha", "beta", "gamma"], "beta"),
                (b"\x1b[1;2A", ["beta", "alpha", "gamma"], "beta"),
                (b"\x1b[1;2B", ["alpha", "beta", "gamma"], "beta"),
                (b"k", ["alpha", "beta", "gamma"], "alpha")):
            session.key(key)
            session.wait(lambda: queue_paths(session) == order and queue_selected(session) == selected,
                         f"queue navigation/reorder failed for {key!r}")
        assert "Pending paused" in session.screen.words()
        assert all((root / name).is_file() for name in ("alpha", "beta", "gamma"))
        session.key(b"d")
        session.wait(lambda: queue_paths(session) == ["beta", "gamma"],
                     "d did not remove only the selected waiting deletion")
        assert (root / "alpha").is_file(), "removing a waiting deletion changed its file"
        close_queue(session)
        session.wait(lambda: deletion_marks(session, ("alpha", "beta", "gamma"))
                     == {"alpha": False, "beta": True, "gamma": True},
                     "removing one waiting job did not clear only its marker")

        fixture_selection(session, "branch", directory=True)
        session.key(b"l")
        session.wait(lambda: fixture_directory(session, root / "branch"), "queued fixture navigation failed")
        enqueue_fixture(session, "leaf")
        session.key(b"<")
        session.wait(lambda: fixture_directory(session, root), "queue overlap check did not return to parent")
        enqueue_fixture(session, "branch", directory=True)
        fixture_selection(session, "branch", directory=True)
        session.key(b"l")
        session.wait(lambda: fixture_directory(session, root / "branch"), "covered leaf navigation failed")
        session.wait(lambda: deletion_marks(session, ("leaf", "other")) == {"leaf": True, "other": True},
                     "queued parent did not mark its independently unqueued descendant")
        enqueue_fixture(session, "leaf")
        session.key(b"<")
        session.wait(lambda: fixture_directory(session, root), "covered leaf check did not return to parent")
        open_queue(session, ["beta", "gamma", "branch"])
        session.key(b"\x1b[F")
        session.wait(lambda: queue_selected(session) == "branch", "queue End did not select parent")
        session.key(b"d")
        session.wait(lambda: queue_paths(session) == ["beta", "gamma"], "queued parent was not removed")
        close_queue(session)
        fixture_selection(session, "branch", directory=True)
        session.key(b"l")
        session.wait(lambda: fixture_directory(session, root / "branch")
                     and deletion_marks(session, ("leaf", "other")) == {"leaf": False, "other": False},
                     "removing queued parent left inherited deletion markers")
        session.key(b"<")
        session.wait(lambda: fixture_directory(session, root), "unqueued descendant check lost parent")
        enqueue_fixture(session, "branch", directory=True)
        open_queue(session, ["beta", "gamma", "branch"])
        assert "Pending paused" in session.screen.words(), "enqueue resumed a paused queue"
        transcript = len(session.raw)
        session.key(b" ")
        session.wait(lambda: not (root / "beta").exists() and not (root / "gamma").exists()
                     and not (root / "branch").exists(), "resumed deletion queue did not finish", timeout=20)
        session.wait(lambda: not queue_paths(session) and "No deletions queued" in session.screen.words(),
                     "completed deletion queue retained a job")
        assert (root / "alpha").is_file(), "queue deleted the removed waiting item"
        assert b"Waiting to scan" not in session.raw[transcript:], "queue restarted the entire scan"
        close_queue(session)
        session.wait(lambda: path_in_header(session.screen, root)
                     and "Scan complete with errors" not in session.screen.text(header=True),
                     "completed queue did not restore an error-free browser")
        elapsed = session.finish(key=b"q")
        return {"case": "deletion-queue", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_queue_resize(binary, work, threads):
    root = work / "queue-resize"
    root.mkdir()
    names = ["alpha", "beta", "gamma"]
    for name in names:
        (root / name).write_bytes(b"queued resize fixture")
    session = Session(binary, root, threads)
    samples = []
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "queue resize fixture scan did not complete")
        pause_empty_queue(session)
        for name in names:
            enqueue_fixture(session, name)

        # A narrow browser has no asynchronous preview publication that could
        # accidentally repair queue rows after their geometry has changed.
        session.resize(9, 80)
        samples.append(stationary_sample(
            session, lambda: session.screen.normal_view()
            and deletion_marks(session, names) == dict.fromkeys(names, True)
            and not preview_rows(session), "narrow paused browser before queue resize"))
        session.key(b"D")

        def queue_ready(paths, selected):
            return (session.screen.has_modal_title("Deletion queue")
                    and "Pending paused" in session.screen.words()
                    and "Updating queue" not in session.screen.words()
                    and queue_paths(session) == paths
                    and queue_selected(session) == selected)

        samples.append(stationary_sample(
            session, lambda: queue_ready(["alpha"], "alpha"),
            "short paused queue before resize"))
        session.resize(20, 80)
        samples.append(stationary_sample(
            session, lambda: queue_ready(names, "alpha"),
            "grown paused queue without input"))
        session.key(b"\x1b[F")
        samples.append(stationary_sample(
            session, lambda: queue_ready(names, "gamma"),
            "last selected paused job before shrink"))
        session.resize(9, 80)
        samples.append(stationary_sample(
            session, lambda: queue_ready(["gamma"], "gamma"),
            "shrunk paused queue keeps selection without input"))
        assert all((root / name).is_file() for name in names), \
            "resizing a paused queue started deletion"
        session.key(b" ")
        session.wait(lambda: all(not (root / name).exists() for name in names)
                     and "No deletions queued" in session.screen.words(),
                     "resized queue did not finish after resuming")
        close_queue(session)
        elapsed = session.finish(key=b"q")
        return {"case": "stationary-paused-queue-resize",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed,
                "input": [], "idle": samples}
    finally:
        session.close()


def check_owned_error_queue(binary, work, threads):
    root = work / "errors"
    root.mkdir()
    for name in ("alpha", "beta", "done"):
        (root / name).write_bytes(b"original")
    session = Session(binary, root, threads)
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "error queue fixture scan did not complete")
        pause_empty_queue(session)
        for name in ("alpha", "beta", "done"):
            enqueue_fixture(session, name)
        # Retain the old inodes so replacement cannot accidentally reuse them.
        for name in ("alpha", "beta"):
            (root / name).rename(work / ("saved-" + name))
            (root / name).write_bytes(b"replacement")
        open_queue(session, ["alpha", "beta", "done"])
        session.key(b" ")
        session.wait(lambda: not (root / "done").exists()
                     and "No deletions queued" in session.screen.words(),
                     "failed targets blocked the rest of the deletion queue", timeout=20)
        assert session.screen.has_modal_title("Deletion queue"), \
            "deletion error replaced an open queue dialog"
        session.key(b"\x1b")
        for name in ("alpha", "beta"):
            path = printable(str((root / name).absolute()))
            session.wait(lambda: session.screen.has_modal_title("Delete failed")
                         and path in session.screen.text(),
                         f"queued failure did not show the full {name} path")
            assert (root / name).read_bytes() == b"replacement", \
                "deletion changed an unconfirmed replacement"
            session.key(b"\r")
        session.wait(lambda: session.screen.normal_view(),
                     "Enter did not dismiss exactly the two queued error reports")
        elapsed = session.finish(key=b"q")
        return {"case": "queued-deletion-errors", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def write_preview_rar(path):
    """One stored RAR4 member; no archive creation utility is required."""
    def block(kind, flags, body):
        header = struct.pack("<BHH", kind, flags, 7 + len(body)) + body
        return struct.pack("<H", zlib.crc32(header) & 0xffff) + header

    name, data = b"inside-rar.txt", b"ARCHIVE_PAYLOAD_MUST_NOT_APPEAR"
    body = struct.pack("<IIBIIBBHI", len(data), len(data), 3,
                       zlib.crc32(data), 0, 20, 0x30, len(name), 0o100644) + name
    path.write_bytes(b"Rar!\x1a\x07\x00" + block(0x73, 0, b"\0" * 6)
                     + block(0x74, 0x8000, body) + data + block(0x7b, 0, b""))


def write_preview_pdf(path):
    """Small valid owned PDF, without depending on a PDF creation utility."""
    stream = b"BT /F1 12 Tf 72 720 Td (PDF preview available) Tj ET\n"
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"endstream",
    ]
    data = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for index, body in enumerate(objects, 1):
        offsets.append(len(data))
        data.extend(f"{index} 0 obj\n".encode() + body + b"\nendobj\n")
    xref = len(data)
    data.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        data.extend(f"{offset:010d} 00000 n \n".encode())
    data.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
    path.write_bytes(data)


def check_owned_helper_shutdown(binary, work, threads):
    # Linux procfs supplies observation identities; retained pidfds make
    # emergency cleanup independent of numeric PID reuse.
    if not sys.platform.startswith("linux"):
        return []
    root = work / "helper-shutdown"
    root.mkdir()
    with zipfile.ZipFile(root / "owned.zip", "w") as archive:
        archive.writestr("inside.txt", "owned helper fixture")
    bindir = work / "helper-shutdown-bin"
    bindir.mkdir()
    pidfile = (work / "owned-helper.pids").resolve()
    helper = bindir / "bsdtar"
    helper.write_text("#!/bin/sh\n/bin/sleep 30 &\nchild=$!\n"
                      "printf '%s %s\\n' \"$$\" \"$child\" > "
                      + shlex.quote(str(pidfile)) + "\nwait\n")
    helper.chmod(0o700)

    def identity(pid):
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
            return fields[0], fields[19]
        except FileNotFoundError:
            return None

    metrics = []
    for label, key, sig in (("q", b"q", None), ("ctrl-c", b"\x03", None),
                            ("sigterm", None, signal.SIGTERM)):
        pidfile.unlink(missing_ok=True)
        session = Session(binary, root, threads, env={"PATH": str(bindir.resolve())})
        owned = {}
        pidfds = []
        try:
            session.resize(28, 160)

            def started():
                return (pidfile.exists() and len(pidfile.read_text().split()) == 2
                        and all(word.isdigit() for word in pidfile.read_text().split()))

            session.wait(started, "owned preview helper and descendant did not start")
            for pid in map(int, pidfile.read_text().split()):
                initial = identity(pid)
                assert initial is not None and initial[0] not in "ZX"
                owned[pid] = initial[1]
                if hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"):
                    fd = os.pidfd_open(pid)
                    current = identity(pid)
                    if current is not None and current[1] == initial[1]:
                        pidfds.append(fd)
                    else:
                        os.close(fd)
            elapsed = session.finish(key=key, sig=sig)

            def alive():
                return [pid for pid, start in owned.items()
                        if (current := identity(pid)) is not None
                        and current[1] == start and current[0] not in "ZX"]

            deadline = time.monotonic() + 0.5
            while alive() and time.monotonic() < deadline:
                time.sleep(0.01)
            assert not alive(), f"{label} left owned preview processes running: {alive()}"
            metrics.append({"case": "preview-helper-shutdown-" + label,
                            "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": []})
        finally:
            session.close()
            for fd in pidfds:
                try:
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                finally:
                    os.close(fd)
    return metrics


def check_owned_special_previews(binary, work, threads):
    root = work / "special-previews"
    root.mkdir()
    outside = work / "preview-targets"
    outside.mkdir()
    (outside / "file").write_text("LINK_FILE_CONTENT_MUST_NOT_APPEAR")
    (outside / "directory").mkdir()
    (outside / "directory" / "LINK_DIRECTORY_CONTENT_MUST_NOT_APPEAR").touch()
    links = {
        "link-file": "../preview-targets/file",
        "link-directory": "../preview-targets/directory",
        "link-dangling": "missing-target/leaf",
        "link-escaped": "missing\n\x1b[31m-target",
        "link-long": "../" + "long-component/" * 8 + "destination-tail",
    }
    for name, target in links.items():
        (root / name).symlink_to(target)
    with zipfile.ZipFile(root / "--owned.zip", "w") as archive:
        archive.writestr("inside-zip.txt", "ARCHIVE_PAYLOAD_MUST_NOT_APPEAR")
    with tarfile.open(root / "owned.tar.gz", "w:gz") as archive:
        data = b"ARCHIVE_PAYLOAD_MUST_NOT_APPEAR"
        entry = tarfile.TarInfo("inside-tar.txt")
        entry.size = len(data)
        archive.addfile(entry, io.BytesIO(data))
    write_preview_rar(root / "owned.rar")
    write_preview_pdf(root / "owned.pdf")
    (root / "unknown.bin").write_bytes(b"\0UNKNOWN_BINARY_PAYLOAD")
    session = Session(binary, root, threads)
    samples = []
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "special preview fixture scan did not finish")

        def pane():
            return "\n".join(preview_rows(session))

        for name, target in links.items():
            fixture_selection(session, name)
            expected = printable(target)
            samples.append(stationary_sample(
                session, lambda: "Links to" in pane()
                and expected in "".join(row.strip() for row in preview_rows(session)),
                f"symlink destination {name}"))
            assert "LINK_FILE_CONTENT_MUST_NOT_APPEAR" not in pane()
            assert "LINK_DIRECTORY_CONTENT_MUST_NOT_APPEAR" not in pane()
            assert path_in_header(session.screen, root), "preview followed a symlink"
        archive_cases = [
            ("--owned.zip", "inside-zip.txt", ("bsdtar", "zipinfo", "unzip", "7zz", "7z")),
            ("owned.tar.gz", "inside-tar.txt", ("bsdtar", "tar", "7zz", "7z")),
            ("owned.rar", "inside-rar.txt", ("bsdtar", "7zz", "7z")),
        ]
        for name, entry, helpers in archive_cases:
            fixture_selection(session, name)
            if any(shutil.which(helper) for helper in helpers):
                samples.append(stationary_sample(
                    session, lambda: "Archive contents" in pane() and entry in pane(),
                    f"installed archive helper {name}"))
            else:
                samples.append(stationary_sample(
                    session, lambda: ("Size" in pane() and "Disk" in pane())
                    or ("Archive contents" in pane() and entry in pane()),
                    f"optional archive preview {name}"))
            assert "ARCHIVE_PAYLOAD_MUST_NOT_APPEAR" not in pane()
        fixture_selection(session, "owned.pdf")
        if shutil.which("pdftotext"):
            samples.append(stationary_sample(
                session, lambda: "PDF preview available" in pane(), "installed PDF helper"))
        else:
            samples.append(stationary_sample(
                session, lambda: "PDF" in pane() and "Size" in pane(), "PDF without helper"))
        fixture_selection(session, "unknown.bin")
        samples.append(stationary_sample(
            session, lambda: "Binary" in pane() and "Size" in pane(), "unsupported binary details"))
        assert "UNKNOWN_BINARY_PAYLOAD" not in pane()
        elapsed = session.finish(key=b"q")
        result = {"case": "symlink-and-system-tool-previews", "first_frame_ms": session.first_frame_ms,
                  "quit_ms": elapsed, "input": [], "idle": samples}
    finally:
        session.close()
    # Availability is per process; no global PATH mutation or installed tools
    # are needed to verify the no-helper behavior.
    empty_path = work / "empty-preview-tool-path"
    empty_path.mkdir()
    session = Session(binary, root, threads, env={"PATH": str(empty_path.resolve())})
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "no-helper preview fixture scan did not finish")
        for name in ("--owned.zip", "owned.tar.gz", "owned.rar", "owned.pdf"):
            fixture_selection(session, name)
            samples.append(stationary_sample(
                session, lambda: "Size" in "\n".join(preview_rows(session))
                and "Disk" in "\n".join(preview_rows(session)),
                f"no-helper details {name}"))
            contents = "\n".join(preview_rows(session))
            assert all(entry not in contents for entry in ("inside-zip.txt", "inside-tar.txt", "inside-rar.txt"))
            assert "PDF preview available" not in contents
        session.finish(key=b"q")
    finally:
        session.close()
    return result


def check_owned_mouse_navigation(binary, work, threads):
    root = work / "mouse-navigation"
    branch = root / "branch"
    (branch / "inner").mkdir(parents=True)
    (root / "plain").touch()
    session = Session(binary, root, threads)
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "mouse navigation fixture scan incomplete")
        fixture_selection(session, "branch", directory=True)
        row = session.screen.selected_row()
        click = f"\x1b[<0;35;{row + 1}M\x1b[<0;35;{row + 1}m".encode()
        session.key(b"v")
        wait_visual(session, ["branch/"], ["branch/"], cursor="branch/")
        session.key(click)
        wait_visual(session, ["branch/"], [], cursor="branch/")
        assert fixture_directory(session, root), "visual click entered a directory"
        session.key(click)
        wait_visual(session, ["branch/"], ["branch/"], cursor="branch/")
        assert fixture_directory(session, root), "visual click entered a directory"
        session.key(b"v")
        session.wait(lambda: path_in_header(session.screen, root),
                     "visual mode did not exit before normal click")
        session.key(click)
        session.wait(lambda: path_in_header(session.screen, branch),
                     "normal directory click did not open the pressed directory")
        normal_style = session.screen.attributes[0][1]
        session.key(b"\x1b[<35;2;1M")
        session.wait(lambda: session.screen.attributes[0][1] != normal_style,
                     "second back-arrow cell did not highlight")
        assert session.screen.attributes[0][0] == session.screen.attributes[0][1], \
            "back-arrow cells did not highlight together"
        assert path_in_header(session.screen, branch), "back hover navigated"
        session.key(b"\x1b[<0;2;1M\x1b[<0;2;1m")
        session.wait(lambda: path_in_header(session.screen, root),
                     "second back-arrow cell did not navigate")
        elapsed = session.finish(key=b"q")
        return {"case": "mouse-click-navigation-and-visual-selection",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_mouse_header(binary, work, threads):
    root = work / "mouse-header"
    branch = root / "branch"
    branch.mkdir(parents=True)
    names = [f"f{index:03d}" for index in range(80)]
    for name in names:
        (branch / name).touch()
    session = Session(binary, root, threads)
    try:
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root), "mouse header scan incomplete")
        assert session.screen.text(header=True).startswith("←─"), "back control missing"
        assert "v select" not in session.screen.text(header=True), "select hint remained"
        assert "[D]" not in session.screen.text(header=True), "idle deletion shortcut remained"
        fixture_selection(session, "branch", directory=True)
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, branch), "branch did not open")
        session.key(b"n")
        settled_home(session, lambda: first_row(session).rstrip().endswith(" " + names[0]),
                     "mouse wheel fixture ordering incomplete")
        session.key(b"\x1b[<35;20;8M")
        session.wait(lambda: session.screen.selected_row() == 7, "hover did not set pointer row")
        before = first_row(session)
        session.key(b"\x1b[<65;20;8M")
        session.wait(lambda: first_row(session) != before and session.screen.selected_row() == 7,
                     "wheel moved highlight instead of the viewport")
        scrolled = first_row(session)
        session.key(b"\x1b[<35;20;8M")
        session.pump(0.15)
        assert first_row(session) == scrolled and session.screen.selected_row() == 7, \
            "hover after wheel moved the viewport or highlight"
        normal_style = session.screen.attributes[0][0]
        session.key(b"\x1b[<35;1;1M")
        session.wait(lambda: session.screen.attributes[0][0] != normal_style,
                     "back control did not highlight on hover")
        assert path_in_header(session.screen, branch), "hover navigated without a click"
        session.key(b"\x1b[<0;1;1M\x1b[<0;1;1m")
        session.wait(lambda: path_in_header(session.screen, root), "back click did not navigate")
        elapsed = session.finish(key=b"q")
    finally:
        session.close()
    long_root = root / ("long-prefix-" + "p" * 100) / "visible-tail"
    long_root.mkdir(parents=True)
    session = Session(binary, long_root, threads)
    try:
        session.resize(28, 60)
        session.wait(lambda: "Scan complete" in session.screen.text(header=True)
                     and "visible-tail" in session.screen.text(header=True),
                     "long header hid the trailing path component")
        assert session.screen.text(header=True).startswith("←─.."), "long path lacked leading omission"
        session.finish(key=b"q")
    finally:
        session.close()
    return {"case": "mouse-wheel-back-and-path-header", "first_frame_ms": session.first_frame_ms,
            "quit_ms": elapsed, "input": []}


def check_owned_cancellation(binary, work, threads):
    root = work / "cancel"
    root.mkdir()
    slow = root / "slow"
    slow.mkdir()
    # A broad real tree gives the PTY a chance to observe active removal.
    # Exact cancellation boundaries are covered by the worker's unit fixture.
    for group in range(32):
        directory = slow / f"group-{group:02d}"
        directory.mkdir()
        for index in range(1024):
            (directory / f"item-{index:04d}").write_bytes(b"")
    waiting = root / "waiting"
    waiting.write_bytes(b"waiting")
    session = Session(binary, root, threads)
    try:
        session.wait(lambda: path_in_header(session.screen, root), "cancellation fixture scan did not complete",
                     timeout=30)
        pause_empty_queue(session)
        enqueue_fixture(session, "slow", directory=True)
        enqueue_fixture(session, "waiting")
        open_queue(session, ["slow", "waiting"])
        transcript = len(session.raw)
        # Resume the first job and immediately pause its successor before
        # waiting for output. A fast worker may finish before cancellation.
        session.key(b"  ")
        session.wait(lambda: "Pending paused" in session.screen.words()
                     and (any(state == "active" and path == "slow"
                              for state, path, _ in queue_rows(session)) or not slow.exists()),
                     "deletion attempt did not preserve paused waiting work")
        session.key(b"c")
        session.wait(lambda: queue_paths(session) == ["waiting"]
                     and "Pending paused" in session.screen.words(),
                     "cancellation attempt lost or started the retained waiting job")
        assert waiting.is_file(), "cancellation deleted the retained waiting item"
        cancellation_observed = slow.is_dir() and any(path.is_file() for path in slow.rglob("*"))
        assert not session.screen.has_modal_title("Delete failed"), "cancellation opened an error notice"
        assert b"Delete failed" not in session.raw[transcript:], "deletion attempt reported an error"
        close_queue(session)
        if not slow.exists():
            session.wait(lambda: not any(row.rstrip().endswith(" slow/")
                                        for row in session.screen.main_rows()),
                         "naturally completed deletion retained its model row")

        session.key(b"q")
        session.wait(lambda: session.screen.has_modal_title("Deletions in progress"),
                     "q did not guard queued deletion on quit")
        session.key(b"n")
        session.wait(lambda: session.screen.normal_view(), "n did not cancel guarded quit")
        assert session.child.poll() is None and waiting.is_file()
        session.key(b"q")
        session.wait(lambda: session.screen.has_modal_title("Deletions in progress"),
                     "guarded quit did not reopen")
        session.resize(4, 24)
        session.wait(lambda: "Ctrl-C" in session.screen.words()
                     and "Scanning stopped." in session.screen.words(),
                     "tiny guarded quit did not expose the interrupt escape hatch")
        session.key(b"q\rn")
        session.wait(lambda: not session.screen.has_modal_border(), "tiny guarded quit did not cancel")
        assert session.child.poll() is None and waiting.is_file(), "unreviewable quit discarded the queue"
        session.resize(28, 100)
        session.wait(lambda: session.screen.normal_view(), "guarded quit did not restore browser geometry")
        session.key(b"\x03")
        wait_quit_warning(session, paused=True)
        elapsed = session.finish(key=b"\x03")
        assert waiting.is_file(), "confirmed quit started a queued deletion"
        if cancellation_observed:
            assert slow.is_dir() and any(path.is_file() for path in slow.rglob("*"))
        return {"case": "deletion-cancellation", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": [], "cancellation_observed": cancellation_observed}
    finally:
        session.close()


def check_owned_quit_guard(binary, work, threads):
    root = work / "quit"
    root.mkdir()
    victim = root / "waiting"
    victim.write_bytes(b"waiting")
    session = Session(binary, root, threads)
    try:
        session.wait(lambda: path_in_header(session.screen, root), "quit fixture scan did not complete")
        pause_empty_queue(session)
        enqueue_fixture(session, "waiting")
        session.key(b"q")
        wait_quit_warning(session, paused=True)
        elapsed = session.finish(key=b"\r")
        assert victim.is_file(), "Enter-confirmed quit started a waiting deletion"
        return {"case": "queued-quit", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def wait_quit_warning(session, paused=False):
    session.wait(lambda: session.screen.has_modal_title("Deletions in progress")
                 and "Finishing deletions. Scanning stopped." in session.screen.words()
                 and "Quits when deletion finishes." in session.screen.words()
                 and (not paused or ("queued" in session.screen.words() and "Paused" in session.screen.words())),
                 "first interrupt did not keep confirmed work and show its shutdown progress")


def check_owned_quit_interrupts(binary, work, threads):
    metrics = []
    for case in ("ctrl-c-resize", "ctrl-c-tiny-burst", "external-sigint"):
        root = work / case
        root.mkdir()
        victim = root / "waiting"
        victim.write_bytes(b"confirmed but paused")
        session = Session(binary, root, threads)
        try:
            session.resize(28, 119)
            session.wait(lambda: path_in_header(session.screen, root),
                         f"{case} fixture scan did not complete")
            pause_empty_queue(session)
            enqueue_fixture(session, "waiting")
            if case == "external-sigint":
                # Observe the first signal before sending another, since Unix
                # may coalesce identical pending signals before delivery.
                session.child.send_signal(signal.SIGINT)
                wait_quit_warning(session, paused=True)
                assert session.child.poll() is None and victim.is_file()
                elapsed = session.finish(sig=signal.SIGINT)
            else:
                if case == "ctrl-c-resize":
                    session.key(b"\x03")
                    wait_quit_warning(session, paused=True)
                    assert session.child.poll() is None and victim.is_file()
                    keys = b"\x03"
                else:
                    # Both interrupts are ready before any warning can paint.
                    keys = b"\x03\x03"
                with stopped_child(session):
                    session.resize(4, 24)
                    session.key(keys)
                    session.wait(lambda: queued_input_bytes(session) == len(keys),
                                 f"{case} interrupts did not reach the stopped child's PTY")
                elapsed = session.finish()
            assert victim.is_file(), f"{case} started a paused deletion during shutdown"
            metrics.append({"case": case, "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": []})
        finally:
            session.close()
    return metrics


def check_owned_quit_completion(binary, work, threads, replacement_error=False):
    root = work / ("quit-replacement-error" if replacement_error else "quit-finish")
    root.mkdir()
    names = [f"item-{index:03d}" for index in range(64)]
    for name in names:
        (root / name).write_bytes(b"confirmed work")
    session = Session(binary, root, threads)
    try:
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root),
                     "finishing-quit fixture scan did not complete")
        pause_empty_queue(session)
        session.key(b"n")
        settled_home(session, lambda: first_row(session).rstrip().endswith(" " + names[0]),
                     "finishing-quit name ordering did not settle")
        session.key(b"v\x1b[1;2F")
        session.wait(lambda: visual_count(session) == len(names)
                     and cursor_path(session, names) == names[-1],
                     "finishing-quit did not select all owned targets")
        open_batch_confirmation(session, count=len(names), key=b"d")
        session.key(b"y")
        session.wait(lambda: session.screen.normal_view() and visual_count(session) is None,
                     "finishing-quit did not enqueue the complete batch")
        assert all((root / name).is_file() for name in names), "paused finishing-quit batch started early"
        session.key(b"D")
        session.wait(lambda: session.screen.has_modal_title("Deletion queue")
                     and "64 queued" in session.screen.words()
                     and "Pending paused" in session.screen.words(),
                     "finishing-quit did not retain its paused batch")
        if replacement_error:
            # Keep every confirmed inode alive, then replace its path while
            # deletion is paused. Identity checks must reject all new files.
            originals = root / "confirmed-originals"
            originals.mkdir()
            confirmed = {name: (root / name).stat() for name in names}
            for name in names:
                (root / name).rename(originals / name)
                (root / name).write_bytes(b"replacement must remain")
                replacement = (root / name).stat()
                assert (replacement.st_dev, replacement.st_ino) != (confirmed[name].st_dev, confirmed[name].st_ino), \
                    "replacement fixture reused a confirmed inode"
        # Each event-loop iteration starts at most two jobs. Make resume and
        # Ctrl-C ready together so work remains when the warning is requested.
        with stopped_child(session):
            keys = b" \x03"
            session.key(keys)
            session.wait(lambda: queued_input_bytes(session) == len(keys),
                         "finishing-quit keys did not reach the stopped child's PTY")
        wait_quit_warning(session)
        # No second interrupt: confirmed work must finish and trigger the exit.
        elapsed = session.finish(expected_exit=1 if replacement_error else 0)
        if replacement_error:
            for name in names:
                assert (root / name).read_bytes() == b"replacement must remain", "queued deletion changed a replacement file"
                assert (originals / name).read_bytes() == b"confirmed work", "queued deletion changed a retained original"
                original = (originals / name).stat()
                assert (original.st_dev, original.st_ino) == (confirmed[name].st_dev, confirmed[name].st_ino), \
                    "queued deletion changed a retained original inode"
            restored = session.raw.rfind(b"\x1b[?1049l")
            assert restored >= 0, "terminal restoration did not precede deletion errors"
            error_output = bytes(session.raw[restored:])
            normalized_error_output = error_output.lower()
            assert (b"ldu: deletion failed:" in normalized_error_output
                    and b"target changed" in normalized_error_output), \
                f"target replacement error was not reported after terminal restoration: {error_output!r}"
            for name in names:
                full_path = printable(str((root / name).absolute())).encode().lower()
                assert full_path in normalized_error_output, \
                    f"completed shutdown omitted the full path for {name}: {error_output!r}"
        else:
            assert all(not (root / name).exists() for name in names), "first interrupt cancelled confirmed work"
        return {"case": "quit-after-target-replacement-error" if replacement_error else "quit-after-confirmed-work-finishes",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def filesystem_snapshot(root):
    """Capture the owned corpus, including directory inode bytes."""
    return {path.relative_to(root): path.lstat()
            for path in [root, *root.rglob("*")]}


def replace_snapshot_scope(previous, root, relative):
    updated = {path: metadata for path, metadata in previous.items()
               if not path.is_relative_to(relative)}
    fresh = filesystem_snapshot(root / relative)
    for path, metadata in fresh.items():
        updated[relative / path] = metadata
    observed = {(metadata.st_dev, metadata.st_ino): metadata for metadata in fresh.values()
                if stat.S_ISREG(metadata.st_mode)}
    # Freshly observed hardlink bytes also change the accepted global owner,
    # even when that owner's directory listing remains untouched.
    for path, metadata in updated.items():
        if stat.S_ISREG(metadata.st_mode):
            updated[path] = observed.get((metadata.st_dev, metadata.st_ino), metadata)
    return updated


def snapshot_totals(snapshot):
    apparent = allocated = 0
    regular = set()
    for metadata in snapshot.values():
        if stat.S_ISREG(metadata.st_mode):
            identity = (metadata.st_dev, metadata.st_ino)
            if identity in regular:
                continue
            regular.add(identity)
        apparent += metadata.st_size
        allocated += metadata.st_blocks * 512
    return apparent, allocated, len(snapshot) - 1


def displayed_size(size):
    units = ("B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB")
    value, unit = float(size), 0
    while value >= 1024 and unit + 1 < len(units):
        value /= 1024
        unit += 1
    if unit == 0:
        return f"{size} B"
    return f"{value:.0f} {units[unit]}" if value >= 100 else f"{value:.1f} {units[unit]}"


def check_snapshot_info(session, snapshot):
    apparent, allocated, items = snapshot_totals(snapshot)
    session.key(b"i")
    session.wait(lambda: session.screen.has_modal_title("Info"),
                 "parent total information did not open")
    words = session.screen.words()
    assert f"Allocated: {displayed_size(allocated)} Apparent: {displayed_size(apparent)}" in words, \
        f"scoped refresh changed retained/global hardlink totals: {words}"
    assert f"Directory · {items} items · 0 errors" in words and "Complete" in words, \
        f"scoped refresh changed retained item/error/completion counts: {words}"
    session.key(b"\x1b")
    session.wait(lambda: session.screen.normal_view(), "parent information did not close")


def refresh_fixture(session, key, current):
    transcript = len(session.raw)
    session.key(key)
    session.wait(lambda: b"Refreshing" in session.raw[transcript:],
                 f"{key!r} did not start a scoped refresh")
    session.wait(lambda: path_in_header(session.screen, current),
                 "scoped refresh did not finish in the current directory")


def check_owned_refresh(binary, work, threads):
    root = work / "refresh"
    tree = root / "tree"
    scope, other = tree / "scope", tree / "other"
    scope.mkdir(parents=True)
    other.mkdir()
    (scope / "old").write_bytes(b"o" * 32768)
    (scope / "shared").write_bytes(b"h" * 49152)
    os.link(scope / "shared", other / "alias")
    (other / "keep").write_bytes(b"k" * 8192)
    expected = filesystem_snapshot(tree)
    session = Session(binary, root, threads)
    try:
        session.wait(lambda: path_in_header(session.screen, root), "refresh fixture scan did not complete")
        fixture_selection(session, "tree", directory=True)
        check_snapshot_info(session, expected)
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, tree), "refresh fixture tree did not open")

        (scope / "old").unlink()
        (scope / "added").write_bytes(b"a" * 65536)
        (scope / "shared").write_bytes(b"h" * 98304)
        os.link(other / "alias", scope / "alias2")
        (other / "unseen").write_bytes(b"u" * 262144)
        expected = replace_snapshot_scope(expected, tree, Path("scope"))
        fixture_selection(session, "scope", directory=True)
        refresh_fixture(session, b"r", tree)
        fixture_selection(session, "scope", directory=True)
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, scope)
                     and "added" in session.screen.text() and "old" not in session.screen.text(),
                     "r did not replace only the selected folder's descendants")

        (scope / "shared").unlink()
        (scope / "alias2").unlink()
        (scope / "added").write_bytes(b"a" * 131072)
        (scope / "new-leaf").write_bytes(b"n" * 32768)
        expected = replace_snapshot_scope(expected, tree, Path("scope"))
        fixture_selection(session, "added")
        refresh_fixture(session, b"R", scope)
        session.wait(lambda: "new-leaf" in session.screen.text()
                     and "shared" not in session.screen.text() and "alias2" not in session.screen.text(),
                     "R did not refresh the current folder with a file selected")
        session.key(b"<")
        session.wait(lambda: path_in_header(session.screen, tree),
                     "current-folder refresh broke parent navigation")
        fixture_selection(session, "other", directory=True)
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, other) and "keep" in session.screen.text(),
                     "untouched sibling did not remain navigable")
        assert "unseen" not in session.screen.text(), "scoped refresh scanned the untouched sibling"
        session.key(b"<")
        session.wait(lambda: path_in_header(session.screen, tree), "sibling parent navigation failed")
        session.key(b"<")
        session.wait(lambda: path_in_header(session.screen, root), "refreshed parent navigation failed")
        check_snapshot_info(session, expected)

        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, tree), "total check did not reenter tree")
        fixture_selection(session, "other", directory=True)
        refresh_fixture(session, b"r", tree)
        fixture_selection(session, "other", directory=True)
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, other) and "unseen" in session.screen.text(),
                     "selected sibling refresh did not discover its previously unseen file")
        session.key(b"<")
        session.wait(lambda: path_in_header(session.screen, tree), "sibling refresh broke parent navigation")
        session.key(b"<")
        session.wait(lambda: path_in_header(session.screen, root), "final parent navigation failed")
        check_snapshot_info(session, filesystem_snapshot(tree))
        elapsed = session.finish(key=b"q")
        return {"case": "scoped-refresh", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_refresh_after_quit(binary, work, threads):
    root = work / "refresh-after-quit"
    victim = root / "victim"
    victim.mkdir(parents=True)
    (victim / "removed").write_bytes(b"r" * 32768)
    (victim / "surviving").write_bytes(b"s" * 4096)
    guard = root / "guard"
    guard.write_bytes(b"paused quit guard")
    session = Session(binary, root, threads)
    samples = []
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "refresh-after-quit fixture scan did not complete")
        pause_empty_queue(session)
        enqueue_fixture(session, "guard")
        session.key(b"q")
        wait_quit_warning(session, paused=True)
        session.key(b"n")
        session.wait(lambda: session.screen.normal_view(),
                     "n did not keep browsing after the quit warning")
        open_queue(session, ["guard"])
        session.key(b"d")
        session.wait(lambda: queue_paths(session) == []
                     and "No deletions queued" in session.screen.words(),
                     "quit-warning guard did not leave the waiting queue")
        close_queue(session)
        assert guard.is_file(), "removing the paused quit guard deleted its file"

        # Discovering this new entry acknowledges the explicit refresh itself;
        # an already-complete header cannot satisfy the observation on its own.
        (root / "refresh-ack").write_bytes(b"manual refresh resumed scanning")
        refresh_fixture(session, b"R", root)
        samples.append(stationary_sample(
            session, lambda: path_in_header(session.screen, root)
            and visible_paths(session, ["refresh-ack"]) == ["refresh-ack"],
            "manual refresh after cancelled quit"))

        enqueue_fixture(session, "victim", directory=True)
        # Simulate survivors deterministically. Removing a waiting target uses
        # the same scans-stopped gate as completion of a cancelled deletion.
        (victim / "removed").unlink()
        (victim / "surviving").write_bytes(b"s" * 131072)
        (victim / "added").write_bytes(b"a" * 65536)
        expected = filesystem_snapshot(victim)
        open_queue(session, ["victim"])
        session.key(b"d")
        session.wait(lambda: queue_paths(session) == []
                     and "No deletions queued" in session.screen.words(),
                     "surviving directory did not leave the waiting queue")
        close_queue(session)
        fixture_selection(session, "victim", directory=True)
        session.key(b"l")
        samples.append(stationary_sample(
            session, lambda: path_in_header(session.screen, victim)
            and set(visible_paths(session, ["removed", "surviving", "added"]))
            == {"surviving", "added"},
            "automatic survivor refresh after manual scanning restart"))
        session.key(b"h")
        session.wait(lambda: path_in_header(session.screen, root),
                     "reconciled directory did not return to its parent")
        fixture_selection(session, "victim", directory=True)
        apparent, allocated, items = snapshot_totals(expected)
        session.key(b"i")
        session.wait(lambda: session.screen.has_modal_title("Info")
                     and f"Allocated: {displayed_size(allocated)} Apparent: {displayed_size(apparent)}"
                     in session.screen.words()
                     and f"Directory · {items} items · 0 errors" in session.screen.words()
                     and "Complete" in session.screen.words(),
                     "restarted reconciliation did not repair survivor totals")
        session.key(b"\x1b")
        session.wait(lambda: session.screen.normal_view(),
                     "reconciled directory information did not close")
        assert guard.is_file() and (victim / "surviving").is_file(), \
            "removing paused jobs deleted their surviving files"
        elapsed = session.finish(key=b"q")
        return {"case": "refresh-restarts-deletion-reconciliation",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed,
                "input": [], "idle": samples}
    finally:
        session.close()


def process_cpu_ticks(pid):
    """Optional Linux observation; never use host scheduling as a pass criterion."""
    try:
        fields = (Path("/proc") / str(pid) / "stat").read_text().rsplit(") ", 1)[1].split()
        return int(fields[11]) + int(fields[12])
    except (OSError, ValueError, IndexError):
        return None


def stationary_sample(session, ready, label, seconds=1.0):
    """Wait for the expected screen and one uninterrupted interval without output."""
    observed = len(session.raw)
    started = time.monotonic()
    initial_ticks = process_cpu_ticks(session.child.pid)

    def quiet():
        nonlocal observed, started, initial_ticks
        if len(session.raw) != observed or not ready():
            # Allow late ordering/preview results, but never accept periodic redraws.
            observed = len(session.raw)
            started = time.monotonic()
            initial_ticks = process_cpu_ticks(session.child.pid)
            return False
        return (session.child.poll() is None
                and time.monotonic() - started >= seconds
                and not select.select([session.master], [], [], 0)[0])

    session.wait(quiet, f"{label} did not reach the expected stationary screen", timeout=15)
    sample = {"phase": label, "seconds": time.monotonic() - started,
              "output_bytes": len(session.raw) - observed,
              "columns": session.screen.cols,
              "header": session.screen.text(header=True).rstrip()}
    assert sample["output_bytes"] == 0, f"{label} emitted output while stationary"
    final_ticks = process_cpu_ticks(session.child.pid)
    if initial_ticks is not None and final_ticks is not None:
        sample["cpu_ticks"] = final_ticks - initial_ticks
        sample["ticks_per_second"] = os.sysconf("SC_CLK_TCK")
    return sample


def check_owned_idle(binary, work, threads):
    root = work / "idle"
    tree, empty = root / "tree", root / "empty"
    tree.mkdir(parents=True)
    empty.mkdir()
    for index in range(64):
        (tree / f"item-{index:03d}").write_bytes(b"i" * 4096)
    (root / "preview.txt").write_text("Idle preview arrived\nsecond idle line\n")
    victim = root / "victim"
    victim.mkdir()
    (victim / "leaf").write_bytes(b"v" * 8192)
    names = ["tree/", "empty/", "preview.txt", "victim/", "added"]
    samples, metrics = [], []
    session = Session(binary, root, threads)
    try:
        session.resize(28, 140)

        def complete(directory=root):
            return (path_in_header(session.screen, directory)
                    and not session.screen.has_modal_border())

        def text_ready():
            return ("Idle preview arrived" in preview_text(session)
                    and "second idle line" in preview_text(session))

        samples.append(stationary_sample(
            session, lambda: complete() and "item-000" in preview_text(session),
            "initial scan with directory preview"))
        fixture_selection(session, "preview.txt")
        samples.append(stationary_sample(
            session, lambda: complete() and text_ready(), "asynchronous text preview"))

        session.key(b"/preview")
        samples.append(stationary_sample(
            session, lambda: "Filter /preview" in session.screen.text(header=True)
            and visible_paths(session, names) == ["preview.txt"] and text_ready(),
            "filtered text preview"))
        session.key(b"\x15zzznomatch")
        samples.append(stationary_sample(
            session, lambda: "Filter /zzznomatch" in session.screen.text(header=True)
            and "No matching entries." in session.screen.text()
            and session.screen.selected_row() is None,
            "filter with no matches"))
        session.key(b"\x1b")
        session.wait(lambda: complete() and len(visible_paths(session, names)) == 4,
                     "clearing idle filter did not restore the entries")
        session.resize(28, 95)
        samples.append(stationary_sample(
            session, lambda: complete() and not preview_rows(session), "narrow browser"))

        (root / "added").write_bytes(b"a" * 16384)
        refresh_fixture(session, b"R", root)
        samples.append(stationary_sample(
            session, lambda: complete() and "added" in visible_paths(session, names),
            "current-directory refresh"))
        fixture_selection(session, "empty", directory=True)
        session.key(b"l")
        samples.append(stationary_sample(
            session, lambda: complete(empty) and session.screen.selected_row() is None,
            "empty directory navigation"))
        session.key(b"h")
        session.wait(lambda: complete(), "empty directory did not return to idle root")

        pause_empty_queue(session)
        enqueue_fixture(session, "victim", directory=True)
        open_queue(session, ["victim"])
        samples.append(stationary_sample(
            session, lambda: session.screen.has_modal_title("Deletion queue")
            and "Pending paused" in session.screen.words()
            and queue_paths(session) == ["victim"],
            "paused deletion queue"))
        assert victim.exists(), "stationary paused queue deleted its target"
        close_queue(session)
        fixture_selection(session, "victim", directory=True)
        # Queue status hides the Refreshing phase. The following ordered modal
        # roundtrip acknowledges r before observing the blocked refresh at rest.
        session.key(b"r?")
        session.wait(lambda: session.screen.has_modal_title("Help"),
                     "help did not acknowledge refresh input for the queued directory")
        session.key(b"\x1b")
        session.wait(lambda: session.screen.normal_view(),
                     "help did not return to the paused refresh browser")
        samples.append(stationary_sample(
            session, lambda: session.screen.normal_view()
            and "Paused" in session.screen.text(header=True)
            and "[D]" in session.screen.text(header=True)
            and deletion_marks(session, ("victim/",)) == {"victim/": True},
            "refresh blocked by paused queued directory"))
        assert (victim / "leaf").exists(), "paused overlapping refresh deleted its target"
        open_queue(session, ["victim"])
        assert "Pending paused" in session.screen.words(), \
            "refresh unexpectedly resumed the paused queue"
        session.key(b" ")
        session.wait(lambda: not victim.exists() and queue_paths(session) == []
                     and "Updating queue" not in session.screen.words(),
                     "resuming idle queue did not complete deletion")
        close_queue(session)
        samples.append(stationary_sample(
            session, lambda: complete() and "victim/" not in visible_paths(session, names),
            "successful deletion"))
        session.resize(28, 140)
        session.wait(lambda: complete(), "wide browser did not redraw after deletion")
        fixture_selection(session, "preview.txt")
        samples.append(stationary_sample(
            session, lambda: complete() and text_ready(), "restored wide text preview"))
        elapsed = session.finish(key=b"q")
        metrics.append({"case": "stationary-after-scan-refresh-delete",
                        "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed,
                        "input": [], "idle": samples})
    finally:
        session.close()

    empty_root = work / "idle-empty"
    empty_root.mkdir()
    session = Session(binary, empty_root, threads)
    try:
        samples = []
        for columns in (140, 95):
            session.resize(28, columns)
            samples.append(stationary_sample(
                session, lambda: path_in_header(session.screen, empty_root)
                and session.screen.selected_row() is None
                and not session.screen.has_modal_border(),
                "empty root"))
        elapsed = session.finish(key=b"q")
        metrics.append({"case": "stationary-empty-root", "first_frame_ms": session.first_frame_ms,
                        "quit_ms": elapsed, "input": [], "idle": samples})
    finally:
        session.close()
    return metrics


def check_owned_leaf_root_idle(binary, work, threads):
    regular = work / "idle-file-root"
    regular.write_bytes(b"Owned regular-file root\n")
    target = work / "idle-link-target"
    target.mkdir()
    sentinel = target / "must-not-follow-root-link"
    sentinel.write_bytes(b"Owned symlink target sentinel\n")
    symlink = work / "idle-symlink-root"
    symlink.symlink_to(target.name, target_is_directory=True)
    cases = (("file", regular, "File"), ("symlink", symlink, "Symlink"),
             ("missing", work / "idle-missing-root", "Unknown"))
    metrics = []
    for label, root, kind in cases:
        session = Session(binary, root, threads)
        samples = []
        try:
            session.resize(28, 140)
            body = ("Partial listing; press i for details." if label == "missing"
                    else "No child entries.")

            def complete():
                return (path_in_header(session.screen, root)
                        and session.screen.selected_row() is None
                        and not session.screen.has_modal_border()
                        and body in session.screen.main_line(1)
                        and ("Scan complete with errors" in session.screen.text(header=True))
                        == (label == "missing"))

            session.wait(complete, f"{label} leaf root did not complete without children")
            session.key(b"i")
            session.wait(lambda: session.screen.has_modal_title("Info")
                         and f"{kind} · 0 items" in session.screen.words(),
                         f"{label} root information did not retain its leaf kind")
            if label == "missing":
                session.wait(
                    lambda: "Latest scan error:" in session.screen.words(),
                    "missing root information omitted the scanner error")
            assert sentinel.name not in session.screen.text(), \
                "symlink root exposed its target's child"
            session.key(b"\x1b")
            samples.append(stationary_sample(
                session, complete, f"{label} root after information dismissal"))

            session.key(b"v")
            samples.append(stationary_sample(
                session, lambda: "Visual" in session.screen.text(header=True)
                and "0 selected" in session.screen.text(header=True)
                and session.screen.selected_row() is None
                and not session.screen.has_modal_border()
                and body in session.screen.main_line(1),
                f"{label} root with empty visual selection"))
            session.key(b"\x1b")
            session.wait(complete, f"{label} root did not leave empty visual mode")

            # Leaf roots have no folder to refresh. Help acknowledges both keys
            # before measuring the unchanged leaf/error state again.
            session.key(b"rR?")
            session.wait(lambda: session.screen.has_modal_title("Help"),
                         f"{label} root did not acknowledge refresh input")
            session.key(b"\x1b")
            samples.append(stationary_sample(
                session, complete, f"{label} root after refresh input"))
            elapsed = session.finish(key=b"q")
            metrics.append({"case": "stationary-" + label + "-root",
                            "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": [], "idle": samples})
        finally:
            session.close()
    assert symlink.is_symlink() and sentinel.read_bytes() == b"Owned symlink target sentinel\n"
    return metrics


def visual_paths(session, names):
    paths = []
    for row, line in enumerate(session.screen.main_rows(), 1):
        line = line.rstrip()
        if (not line or line[0] not in "›✓"
                or sum(background == 4 for background, _ in session.screen.attributes[row][:16]) < 3):
            continue
        name = next((name for name in names if line.endswith(" " + name)), None)
        if name is not None:
            paths.append(name)
    return paths


def cursor_path(session, names):
    paths = [name for line in session.screen.main_rows() if line.startswith("›")
             for name in names if line.rstrip().endswith(" " + name)]
    return paths[0] if len(paths) == 1 else None


def wait_visual(session, names, expected, cursor=None):
    session.wait(lambda: "Visual" in session.screen.text(header=True)
                 and visual_count(session) == len(expected)
                 and visual_paths(session, names) == expected
                 and (cursor is None or cursor_path(session, names) == cursor),
                 f"visual selection did not contain {expected} with cursor {cursor}")


def reset_fixture_visual(session, names):
    session.key(b"v")
    session.wait(lambda: "Visual" not in session.screen.text(header=True)
                 and not visual_paths(session, names), "v did not clear visual selection")
    fixture_selection(session, "alpha")
    session.key(b"V")
    wait_visual(session, names, ["alpha"])


def batch_count(session):
    match = re.search(r"Permanently remove (\d+) selected entries in:", session.screen.words())
    return int(match[1]) if match else None


def wait_batch_confirmation(session, count=2):
    session.wait(lambda: session.screen.has_modal_title("Delete selection?")
                 and batch_count(session) == count
                 and "y / Enter: enqueue selected" in session.screen.words(),
                 "batch confirmation did not finish checking selected targets")


def open_batch_confirmation(session, count=2, key=b"D"):
    session.key(key)
    wait_batch_confirmation(session, count)


def visual_count(session):
    match = re.search(r"Visual · (\d+) selected", session.screen.text(header=True))
    return int(match[1]) if match else None


def check_owned_drag_modes(binary, work, threads, include_control=True):
    """Exercise separate mouse reports with actual painted pauses between them."""
    root = work / "drag-modes"
    root.mkdir()
    names = [f"item-{index:02d}" for index in range(8)]
    for name in names:
        (root / name).touch()
    session = Session(binary, root, threads)
    samples = []
    try:
        session.resize(15, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "drag fixture scan did not finish")
        session.key(b"n")
        settled_home(session, lambda: visible_paths(session, names) == names,
                     "drag fixture name order did not settle")
        session.key(b"v")
        wait_visual(session, names, names[:1], cursor=names[0])

        def event(code, index, released, selected, label):
            # Header is terminal row one; index zero is SGR row two.
            suffix = "m" if released else "M"
            session.key(f"\x1b[<{code};35;{index + 2}{suffix}".encode())
            expected = [names[i] for i in selected]
            samples.append(stationary_sample(
                session, lambda: "Visual" in session.screen.text(header=True)
                and visible_paths(session, names) == names
                and visual_count(session) == len(expected)
                and visual_paths(session, names) == expected
                and cursor_path(session, names) == names[index], label))

        # A frame and quiet interval separate every report. In particular, a
        # held plain press must retain its baseline after its toggle is drawn.
        event(0, 2, False, [0, 2], "plain held press")
        event(32, 5, False, [0, 2, 3, 4, 5], "plain drag extends")
        event(32, 3, False, [0, 2, 3], "plain drag reverses")
        event(0, 3, True, [0, 2, 3], "plain drag releases")
        if include_control:
            # Start on an unmarked row: subtraction must never add that row.
            event(16, 1, False, [0, 2, 3], "Ctrl held unmarked press")
            event(48, 3, False, [0], "Ctrl drag subtracts")
            event(48, 2, False, [0, 3], "Ctrl reversal restores baseline")
            # The mode belongs to the press, even when Ctrl is released early.
            event(32, 4, False, [0], "Ctrl drag keeps press modifier")
            event(0, 4, True, [0], "Ctrl drag releases")
            event(16, 5, False, [0], "Ctrl click unmarked press")
            event(0, 5, True, [0], "Ctrl click unmarked release")
            event(16, 0, False, [], "Ctrl click marked press")
            event(0, 0, True, [], "Ctrl click marked release")
        elapsed = session.finish(key=b"q")
        return {"case": "visual-drag-modes", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": [], "idle": samples}
    finally:
        session.close()


def check_owned_visual(binary, work, threads):
    root = work / "visual"
    root.mkdir()
    names = ["alpha", "beta", "gamma", "omega"]
    for name in names:
        (root / name).write_bytes(b"v" * 4096)
    session = Session(binary, root, threads)
    try:
        session.wait(lambda: path_in_header(session.screen, root), "visual fixture scan did not complete")
        assert b"\x1b[?1000h" in session.raw, "visual browser did not enable mouse input"
        pause_empty_queue(session)
        session.key(b"n")
        session.wait(lambda: visible_paths(session, names) == names,
                     "visual name ordering did not settle")
        fixture_selection(session, "alpha")
        session.key(b"v")
        wait_visual(session, names, ["alpha"], cursor="alpha")
        session.key(b"\x1b[1;2B")
        wait_visual(session, names, ["alpha", "beta"], cursor="beta")
        session.key(b"\x1b[1;2B")
        wait_visual(session, names, ["alpha", "beta", "gamma"], cursor="gamma")
        session.key(b"\r")
        wait_visual(session, names, ["alpha", "beta"], cursor="gamma")
        for key, cancel in ((b"d", b"n"), (b"\x1b[3~", b"\x1b")):
            open_batch_confirmation(session, key=key)
            session.key(cancel)
            session.wait(lambda: session.screen.normal_view(), "batch deletion confirmation did not cancel")
            wait_visual(session, names, ["alpha", "beta"])
            assert all((root / name).is_file() for name in names), "cancelled batch deletion changed files"
        open_batch_confirmation(session)
        session.resize(4, 24)
        session.wait(lambda: "Resize to review" in session.screen.words(),
                     "tiny batch dialog did not disable confirmation")
        session.key(b"y\rn")
        session.wait(lambda: not session.screen.has_modal_border(), "tiny batch dialog did not cancel")
        assert all((root / name).is_file() for name in names), "unreviewable batch confirmation changed files"
        session.resize(28, 100)
        session.wait(lambda: session.screen.normal_view(), "batch dialog did not restore browser geometry")
        wait_visual(session, names, ["alpha", "beta"])

        reset_fixture_visual(session, names)
        session.key(b"\x1b[<4;35;4M\x1b[<4;35;4m")
        wait_visual(session, names, ["alpha", "beta", "gamma"])
        reset_fixture_visual(session, names)
        session.key(b"\x1b[<4;35;3M\x1b[<36;35;4M\x1b[<4;35;4m")
        wait_visual(session, names, ["alpha", "beta", "gamma"])
        reset_fixture_visual(session, names)
        session.key(b"\x1b[<4;35;2M\x1b[<36;35;3M\x1b[<4;35;3m")
        wait_visual(session, names, ["beta"])
        reset_fixture_visual(session, names)
        # A plain drag adds to the marks captured before the press.
        session.key(b"\x1b[<0;35;3M\x1b[<32;35;4M\x1b[<0;35;4m")
        wait_visual(session, names, ["alpha", "beta", "gamma"])
        reset_fixture_visual(session, names)
        # Releasing Shift before the next motion must retain inverse semantics.
        session.key(b"\x1b[<4;35;2M\x1b[<32;35;3M\x1b[<0;35;3m")
        wait_visual(session, names, ["beta"])
        reset_fixture_visual(session, names)
        session.key(b"\x1b[1;2B")
        wait_visual(session, names, ["alpha", "beta"])

        open_batch_confirmation(session)
        session.key(b"y")
        session.wait(lambda: session.screen.normal_view()
                     and "Visual" not in session.screen.text(header=True),
                     "confirmed batch deletion did not finish enqueueing and exit visual mode")
        assert all((root / name).is_file() for name in names), "batch enqueue ignored pending pause"
        open_queue(session, ["alpha", "beta"])
        session.key(b" ")
        session.wait(lambda: not (root / "alpha").exists() and not (root / "beta").exists(),
                     "batch queue did not remove both selected files")
        session.wait(lambda: not queue_paths(session) and "No deletions queued" in session.screen.words(),
                     "batch queue did not finish")
        assert all((root / name).is_file() for name in ("gamma", "omega")), "batch queue removed unselected files"
        close_queue(session)
        elapsed = session.finish(key=b"q")
        return {"case": "visual-batch-deletion", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def visible_paths(session, names):
    """Known fixture basenames in painted order; directory names include '/'."""
    return [name for row in session.screen.main_rows()
            for name in names if row.rstrip().endswith(" " + name)]


def check_owned_input_modes(binary, work, threads):
    root = work / "input-modes"
    root.mkdir()
    names = [f"item-{index:03d}" for index in range(64)]
    for name in names:
        (root / name).touch()
    session = Session(binary, root, threads)
    try:
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root),
                     "input-mode fixture scan did not complete")
        session.key(b"n")
        settled_home(session, lambda: first_row(session).rstrip().endswith(" " + names[0]),
                     "input-mode name ordering did not settle")
        page = session.screen.rows - 1

        def moved(key, index, marks=None):
            session.key(key)
            session.wait(lambda: cursor_path(session, names) == names[index]
                         and visual_count(session) == marks,
                         f"input {key!r} did not reach {names[index]} with {marks} marks")

        moved(b"\x1b[B", 1)
        moved(b"\x1b[1;2B", 1 + page)
        moved(b"\x1b[1;2A", 1)
        moved(b"\x1b[H", 0)
        session.key(b"v")
        wait_visual(session, names, names[:1], cursor=names[0])
        # Every plain movement retains the original mark, including movements
        # that take it outside the viewport. Home exposes it again for inspection.
        moved(b"\x1b[B", 1, 1)
        wait_visual(session, names, names[:1], cursor=names[1])
        moved(b"\x1b[F", len(names) - 1, 1)
        moved(b"\x1b[H", 0, 1)
        wait_visual(session, names, names[:1], cursor=names[0])
        moved(b"\x1b[6~", page, 1)
        moved(b"\x1b[5~", 0, 1)
        wait_visual(session, names, names[:1], cursor=names[0])
        moved(b"\x1b[1;2B", 1, 2)
        wait_visual(session, names, names[:2], cursor=names[1])
        moved(b"\x1b[1;2B", 2, 3)
        wait_visual(session, names, names[:3], cursor=names[2])
        # Reversing an anchored range restores its original baseline.
        moved(b"\x1b[1;2A", 1, 2)
        wait_visual(session, names, names[:2], cursor=names[1])
        moved(b"\x1b[1;2B", 2, 3)
        session.key(b"\r")
        wait_visual(session, names, names[:2], cursor=names[2])
        moved(b"\x1b[B", 3, 2)
        moved(b"\x1b[A", 2, 2)
        wait_visual(session, names, names[:2], cursor=names[2])
        session.key(b"\r")
        wait_visual(session, names, names[:3], cursor=names[2])
        session.key(b"\r")
        wait_visual(session, names, names[:2], cursor=names[2])
        moved(b"\x1b[1;2B", 3, 3)
        wait_visual(session, names, [names[0], names[1], names[3]], cursor=names[3])
        session.key(b"\x1b")
        session.wait(lambda: path_in_header(session.screen, root) and visual_count(session) is None,
                     "Escape did not leave visual mode")
        elapsed = session.finish(key=b"q")
        return {"case": "normal-and-visual-input-modes", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_filter(binary, work, threads):
    root = work / "ancestor-needle"
    root.mkdir()
    files = ["alpha-report.txt", "azure-parts.log", "quartz.txt"]
    for name in files:
        # Distinguish the initial size order from the requested name order.
        (root / name).write_bytes(b"q" * 8192 if name == "quartz.txt" else b"")
    archive, container = root / "Archive", root / "container"
    archive.mkdir()
    container.mkdir()
    (archive / "unrelated-child.bin").touch()
    (container / "deep-needle.txt").touch()
    names = files + ["Archive/", "container/"]
    known = names + ["unrelated-child.bin", "deep-needle.txt"]
    session = Session(binary, root, threads)
    try:
        # Match only main-list rows; the Contents pane may show descendants.
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root),
                     "filter fixture scan did not complete")
        session.key(b"n")
        # Resorting preserves quartz's physical row and can leave the viewport
        # at the last item. Home must expose the complete name-sorted baseline.
        settled_home(session, lambda: len(visible_paths(session, known)) == len(names)
                     and set(visible_paths(session, known)) == set(names)
                     and visible_paths(session, known)[-1] == "quartz.txt",
                     "filter fixture did not expose all immediate entries in name order")
        baseline = visible_paths(session, known)

        def editing(query, expected):
            session.wait(lambda: f"Filter /{query}" in session.screen.text(header=True)
                         and visible_paths(session, known) == expected
                         and not session.screen.has_modal_border()
                         and (cursor_path(session, known) in expected if expected else
                              "No matching entries." in session.screen.text()
                              and session.screen.selected_row() is None),
                         f"live filter {query!r} did not show exactly {expected}")

        def cleared():
            session.wait(lambda: path_in_header(session.screen, root)
                         and visible_paths(session, known) == baseline
                         and cursor_path(session, names) in names,
                         "clearing the filter did not restore the immediate entries")

        def applied(query, expected, directory=root):
            session.wait(lambda: fixture_directory(session, directory)
                         and f"/{query}" in session.screen.text(header=True)
                         and "Filter /" not in session.screen.text(header=True)
                         and visible_paths(session, known) == expected
                         and cursor_path(session, known) in expected,
                         f"Enter did not finish editing {query!r} in the same directory")

        matches = [name for name in baseline if name in files[:2]]
        session.key(b"/APT")
        editing("APT", matches)
        session.key(b"z")
        editing("APTz", [])
        session.key(b"\x7f")
        editing("APT", matches)
        session.key(b"\x15")
        editing("", baseline)
        session.key(b"q")
        editing("q", ["quartz.txt"])
        session.key(b"\x1b")
        cleared()

        session.key(b"/deepneedle")
        editing("deepneedle", [])
        session.key(b"\x15ancestor-needle")
        editing("ancestor-needle", [])
        session.key(b"\x1b")
        cleared()
        session.key(b"/APT")
        editing("APT", matches)
        session.key(b"\r")
        applied("APT", matches)
        session.key(b"\x1b")
        cleared()

        session.key(b"/aRc")
        editing("aRc", ["Archive/"])
        session.key(b"\r")
        applied("aRc", ["Archive/"])
        session.key(b"l")
        session.wait(lambda: path_in_header(session.screen, archive)
                     and visible_paths(session, known) == ["unrelated-child.bin"],
                     "entering a filtered directory did not clear its old query")
        session.key(b"/unb")
        editing("unb", ["unrelated-child.bin"])
        session.key(b"\r")
        applied("unb", ["unrelated-child.bin"], archive)
        session.key(b"h")
        cleared()
        elapsed = session.finish(key=b"q")
        return {"case": "current-directory-fuzzy-filter", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_batch_queue(binary, work, threads):
    root = work / "batch-fifo"
    root.mkdir()
    victims = [root / f"item-{index:02d}" for index in range(32)]
    for victim in victims:
        victim.write_bytes(b"confirmed FIFO job")
    review = root / "review"
    review.write_bytes(b"unconfirmed batch target")
    session = Session(binary, root, threads)
    try:
        session.resize(28, 119)
        session.wait(lambda: path_in_header(session.screen, root),
                     "batch FIFO fixture scan did not complete")
        pause_empty_queue(session)
        session.key(b"n")
        settled_home(session, lambda: first_row(session).rstrip().endswith(" item-00"),
                     "batch FIFO name ordering did not settle")
        session.key(b"v" + b"\x1b[1;2B" * 31)
        session.wait(lambda: visual_count(session) == 32
                     and cursor_path(session, ["item-31"]) == "item-31",
                     "batch FIFO did not select exactly its 32 confirmed targets")
        open_batch_confirmation(session, count=32, key=b"d")
        session.key(b"y")
        session.wait(lambda: session.screen.normal_view() and visual_count(session) is None,
                     "confirmed FIFO batch did not return to browsing")
        assert all(victim.is_file() for victim in victims), "FIFO batch ignored the paused queue"
        session.key(b"\x1b[F")
        session.wait(lambda: cursor_path(session, ["review"]) == "review",
                     "batch FIFO did not select the independent review target")
        session.key(b"D")
        session.wait(lambda: session.screen.has_modal_title("Deletion queue")
                     and "32 queued" in session.screen.words()
                     and "Pending paused" in session.screen.words(),
                     "batch FIFO did not retain all 32 paused jobs")
        assert all(victim.is_file() for victim in victims), "paused FIFO changed a confirmed target"
        # Each event-loop iteration handles one key and can start at most two
        # jobs. Queue all four keys before resuming our child, so older confirmed
        # work must remain when d opens review, regardless of deletion speed.
        with stopped_child(session):
            keys = b" Dvd"
            session.key(keys)
            session.wait(lambda: queued_input_bytes(session) == len(keys),
                         "batch FIFO keys did not enter the stopped child's PTY")
        wait_batch_confirmation(session, count=1)
        assert review.is_file(), "unconfirmed batch changed its target"
        # Send no more input while the confirmed jobs drain: reviewing C must
        # not pause the FIFO or authorize C's deletion.
        session.wait(lambda: all(not victim.exists() for victim in victims)
                     and session.screen.has_modal_title("Delete selection?")
                     and batch_count(session) == 1
                     and "y / Enter: enqueue selected" in session.screen.words(),
                     "unconfirmed batch paused previously confirmed FIFO work")
        assert review.is_file(), "FIFO completion deleted the unconfirmed batch target"
        session.key(b"n")
        session.wait(lambda: session.screen.normal_view() and visual_count(session) == 1,
                     "cancelling the batch lost its retained selection")
        session.key(b"v")
        session.wait(lambda: session.screen.normal_view() and visual_count(session) is None,
                     "batch FIFO did not leave visual mode")
        open_queue(session, [])
        session.wait(lambda: "No deletions queued" in session.screen.words(),
                     "completed FIFO retained an active deletion")
        close_queue(session)
        elapsed = session.finish(key=b"q")
        return {"case": "confirmed-fifo-during-batch-review",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed, "input": []}
    finally:
        session.close()



def check_owned_completion_input(binary, work, threads):
    root = work / "completion-input"
    root.mkdir()
    names = [f"item-{index:03d}" for index in range(128)]
    victims = [root / name for name in names]
    for victim in victims:
        victim.write_bytes(b"confirmed completion fixture")
    query = "abcdefghijklmnopqrstuvwxyz0123456789"
    survivor = root / ("zz-" + query)
    survivor.write_bytes(b"unselected input sentinel")
    session = Session(binary, root, threads)
    try:
        session.resize(28, 160)
        session.wait(lambda: path_in_header(session.screen, root),
                     "completion/input fixture scan did not complete")
        pause_empty_queue(session)
        session.key(b"n")
        settled_home(session, lambda: first_row(session).rstrip().endswith(" " + names[0]),
                     "completion/input name ordering did not settle")
        session.key(b"v\x1b[1;2F")
        session.wait(lambda: visual_count(session) == len(names) + 1
                     and cursor_path(session, [survivor.name]) == survivor.name,
                     "completion/input did not select the full fixture")
        session.key(b"\r")
        session.wait(lambda: visual_count(session) == len(names),
                     "completion/input did not exclude its retained sentinel")
        open_batch_confirmation(session, count=len(names), key=b"d")
        session.key(b"y")
        session.wait(lambda: session.screen.normal_view() and visual_count(session) is None,
                     "completion/input did not finish admitting its paused batch")
        assert all(victim.is_file() for victim in victims), "paused completion/input batch started early"
        session.key(b"D")
        session.wait(lambda: session.screen.has_modal_title("Deletion queue")
                     and f"{len(names)} queued" in session.screen.words()
                     and "Pending paused" in session.screen.words(),
                     "completion/input did not retain every paused target")
        # Input is already buffered when deletion starts. There are more jobs
        # than twice the key count, so per-event dispatch cannot exhaust the
        # queue before the burst is handled. Every query character must survive
        # completion wakes, and the remaining jobs must finish without input.
        with stopped_child(session):
            keys = b" D/" + query.encode("ascii")
            session.key(keys)
            session.wait(lambda: queued_input_bytes(session) == len(keys),
                         "completion/input burst did not reach the stopped child's PTY")
        session.wait(lambda: queued_input_bytes(session) == 0
                     and f"Filter /{query}" in session.screen.text(header=True)
                     and visible_paths(session, [survivor.name]) == [survivor.name],
                     "deletion completion wakes lost buffered filter input")
        session.wait(lambda: all(not victim.exists() for victim in victims)
                     and f"Filter /{query}" in session.screen.text(header=True),
                     "queued deletions stopped after buffered input drained", timeout=20)
        assert survivor.read_bytes() == b"unselected input sentinel", \
            "completion/input deleted or changed the unselected sentinel"
        session.key(b"\x1b")
        session.wait(lambda: path_in_header(session.screen, root)
                     and cursor_path(session, [survivor.name]) == survivor.name,
                     "completion/input filter did not clear after the queue drained")
        open_queue(session, [])
        session.wait(lambda: "No deletions queued" in session.screen.words(),
                     "completion/input retained a completed deletion job")
        close_queue(session)
        elapsed = session.finish(key=b"q")
        return {"case": "deletion-completions-during-buffered-input",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def check_owned_visual_ranges(binary, work, threads):
    root = work / "visual-range"
    root.mkdir()
    names = [f"item-{index:05d}" for index in range(8192)]
    for name in names:
        (root / name).touch()
    session = Session(binary, root, threads)
    try:
        session.wait(lambda: path_in_header(session.screen, root)
                     and "Scan complete" in session.screen.text(header=True),
                     "large visual fixture scan did not complete", timeout=30)
        session.key(b"n")
        # Scan completion precedes the ordering worker's final publication.
        # A fresh complete selection proves the whole cache is ready; retries
        # belong only to setup, never to the deferred-delete regression below.
        deadline = time.monotonic() + 30
        def ready(predicate, description):
            remaining = deadline - time.monotonic()
            assert remaining > 0, "large visual fixture cache never became complete"
            session.wait(predicate, description, timeout=remaining)
        while True:
            session.key(b"\x1b[H")
            ready(lambda: cursor_path(session, names[:1]) == names[0],
                  "large visual fixture Home did not reach the first sorted row")
            session.key(b"v")
            ready(lambda: visual_count(session) == 1,
                  "large visual fixture did not start a fresh selection")
            # Help acknowledges Shift-End even for a one-row partial
            # cache. Closing it preserves selection and allows pumping to finish.
            session.key(b"\x1b[1;2F?")
            ready(lambda: session.screen.has_modal_title("Help"),
                  "large visual fixture did not acknowledge setup End")
            session.key(b"\x1b")
            ready(lambda: session.screen.normal_view()
                  and visual_count(session) is not None
                  and "Selecting" not in session.screen.text(header=True),
                  "large visual fixture setup selection did not settle")
            complete = visual_count(session) == len(names)
            session.key(b"v")
            ready(lambda: session.screen.normal_view() and visual_count(session) is None,
                  "large visual fixture did not clear its setup selection")
            if complete:
                break
        session.key(b"\x1b[H")
        ready(lambda: cursor_path(session, names[:1]) == names[0],
              "large visual fixture Home did not return to the first sorted row")
        session.key(b"v")
        ready(lambda: visual_count(session) == 1,
              "large visual fixture did not enter visual mode")
        # Deliver deletion and an early Enter before an 8192-row range can
        # finish. Enter must not cancel the waiting request or confirm unseen
        # targets. A second delete key would mask a discarded request.
        with stopped_child(session):
            keys = b"\x1b[1;2Fd\r"
            session.key(keys)
            unread = lambda: struct.unpack("I", fcntl.ioctl(
                session.slave, termios.FIONREAD, struct.pack("I", 0)))[0]
            session.wait(lambda: unread() == len(keys),
                         "large range keys did not enter the stopped child's PTY")
        wait_batch_confirmation(session, len(names))
        session.key(b"n")
        session.wait(lambda: session.screen.normal_view()
                     and visual_count(session) == len(names)
                     and names[-1] in visual_paths(session, names[-session.screen.rows:]),
                     "cancelling pending-range deletion did not preserve all marks")

        session.key(b"v")
        session.wait(lambda: path_in_header(session.screen, root),
                     "large visual fixture did not exit visual mode")
        session.key(b"\x1b[H")
        session.wait(lambda: first_row(session).rstrip().endswith(" " + names[0])
                     and session.screen.selected_row() == 1,
                     "edge drag did not start at the first row")
        session.key(b"v")
        session.wait(lambda: visual_count(session) == 1,
                     "edge drag did not enter visual mode")
        # Hold a plain drag on the last list row without further motion. The
        # timer must scroll and extend selection beyond the initial viewport.
        edge = session.screen.rows
        session.key(f"\x1b[<0;35;2M\x1b[<32;35;{edge}M".encode())
        session.wait(lambda: (visual_count(session) or 0) > session.screen.rows + 12
                     and not first_row(session).rstrip().endswith(" " + names[0]),
                     "held edge drag did not select beyond the viewport")
        session.key(f"\x1b[<0;35;{edge}md".encode())
        session.wait(lambda: session.screen.has_modal_title("Delete selection?")
                     and (batch_count(session) or 0) > session.screen.rows
                     and "y / Enter: enqueue selected" in session.screen.words(),
                     "released edge drag did not produce a complete batch selection")
        selected = batch_count(session)
        session.key(b"\x1b")
        session.wait(lambda: session.screen.normal_view()
                     and visual_count(session) == selected,
                     "cancelling edge-drag deletion did not preserve marks")
        assert len(list(root.iterdir())) == len(names), "cancelled range deletion changed files"
        elapsed = session.finish(key=b"q")
        return {"case": "visual-pending-range-and-edge-drag",
                "first_frame_ms": session.first_frame_ms, "quit_ms": elapsed, "input": []}
    finally:
        session.close()


@contextmanager
def stopped_child(session):
    """Always resume only this harness's child after a controlled stop."""
    session.child.send_signal(signal.SIGSTOP)
    try:
        deadline = time.monotonic() + 5
        while True:
            child, status = os.waitpid(session.child.pid, os.WUNTRACED | os.WNOHANG)
            if child:
                if not os.WIFSTOPPED(status):
                    session.child.returncode = os.waitstatus_to_exitcode(status)
                    raise AssertionError(f"owned PTY child exited before resize/input fixture: {status}")
                break
            if time.monotonic() >= deadline:
                raise AssertionError("owned PTY child did not acknowledge SIGSTOP")
            session.pump(0.01)
        yield
    finally:
        if session.child.poll() is None:
            session.child.send_signal(signal.SIGCONT)


def stopped_resize_key(session, rows, cols, key):
    """Queue simultaneous resize/input readiness while only our child is stopped."""
    with stopped_child(session):
        session.resize(rows, cols)
        session.key(key)


def check_owned_resize_confirmation(binary, work, threads, cases=("single", "batch", "quit")):
    metrics = []
    for case in cases:
        root = work / ("resize-" + case)
        root.mkdir()
        names = ["alpha", "beta"]
        for name in names:
            (root / name).write_bytes(b"resize confirmation")
        session = Session(binary, root, threads)
        try:
            session.resize(28, 119)
            session.wait(lambda: path_in_header(session.screen, root),
                         f"{case} resize confirmation fixture scan did not complete")
            pause_empty_queue(session)
            fixture_selection(session, "alpha")
            if case == "single":
                session.key(b"d")
                session.wait(lambda: session.screen.has_modal_title("Delete?")
                             and "alpha" in session.screen.text()
                             and "y / Enter: delete" in session.screen.words(),
                             "single resize confirmation did not become reviewable")
            elif case == "batch":
                session.key(b"v")
                wait_visual(session, names, ["alpha"], cursor="alpha")
                open_batch_confirmation(session, count=1, key=b"d")
            elif case == "quit":
                enqueue_fixture(session, "alpha")
                session.key(b"q")
                session.wait(lambda: session.screen.has_modal_title("Deletions in progress")
                             and "q / Enter: cancel all and quit" in session.screen.words(),
                             "quit resize confirmation did not become reviewable")
            else:
                raise AssertionError(f"unknown resize confirmation case: {case}")

            # Make both input and SIGWINCH ready before the owned child resumes.
            # Do not wait for a refusal redraw before sending confirmation: the
            # terminal parser may return the keys before the pending resize.
            keys = b"q\r" if case == "quit" else b"y\r"
            with stopped_child(session):
                session.resize(4, 24)
                session.key(keys)
                session.wait(lambda: queued_input_bytes(session) == len(keys),
                             f"{case} confirmation burst did not reach stopped child's PTY")
            tiny_message = "Scanning stopped." if case == "quit" else "Resize to review"
            session.wait(lambda: tiny_message in session.screen.words()
                         and queued_input_bytes(session) == 0,
                         f"{case} confirmation accepted stale geometry before resize redraw",
                         timeout=5)
            assert session.child.poll() is None, f"tiny {case} confirmation exited"
            assert all((root / name).is_file() for name in names), f"tiny {case} confirmation removed a file"
            session.key(b"n")
            session.wait(lambda: not session.screen.has_modal_border(),
                         f"tiny {case} confirmation did not cancel")
            session.resize(28, 119)
            session.wait(lambda: session.screen.normal_view(),
                         f"{case} confirmation did not restore browser geometry")
            if case == "batch":
                wait_visual(session, names, ["alpha"], cursor="alpha")
                session.key(b"v")
                session.wait(lambda: visual_count(session) is None,
                             "resize batch cancellation did not leave visual mode")
            expected = ["alpha"] if case == "quit" else []
            open_queue(session, expected)
            assert "Pending paused" in session.screen.words(), "resize confirmation changed queue pause"
            assert all((root / name).is_file() for name in names), "resize confirmation changed fixture files"
            close_queue(session)
            elapsed = session.finish(key=b"q\r" if case == "quit" else b"q")
            metrics.append({"case": "simultaneous-resize-" + case + "-confirmation",
                            "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": []})
        finally:
            session.close()
    return metrics


def prepare_input_fixture(session, root, label):
    def ordered_rows():
        return (path_in_header(session.screen, root)
                and session.screen.main_line(1).rstrip().endswith(" folder/")
                and session.screen.main_line(2).rstrip().endswith(" last-file"))

    session.wait(lambda: path_in_header(session.screen, root)
                 and "Scan complete" in session.screen.text(header=True),
                 f"{label} fixture scan did not complete")
    # Initial cursor anchoring can leave the first row above the viewport.
    # Name order stays fixed as late size snapshots arrive.
    session.key(b"n")
    session.key(b"\x1b[H")
    session.wait(ordered_rows, f"{label} fixture name order did not settle")
    # Consume an actual End/Home round trip after observing both final rows.
    session.key(b"\x1b[F")
    session.wait(lambda: ordered_rows() and session.screen.selected_row() == 2,
                 f"{label} fixture End did not select last-file")
    session.key(b"\x1b[H")
    session.wait(lambda: ordered_rows() and session.screen.selected_row() == 1,
                 f"{label} fixture Home did not select folder")


def check_owned_held_resize(binary, work, threads, cases=("normal", "visual")):
    """Keep captured list rows fixed while a held press is clipped by resize."""
    metrics = []
    for mode in cases:
        assert mode in ("normal", "visual"), mode
        root = work / ("held-resize-" + mode)
        root.mkdir()
        names = [f"item-{index:03d}" for index in range(24)]
        for name in names:
            (root / name).touch()
        session = Session(binary, root, threads)
        samples = []
        try:
            # No preview publication can accidentally repair the viewport after
            # a resize. There are more entries than either list can display.
            session.resize(11, 80)
            session.wait(lambda: "Scan complete" in session.screen.text(header=True)
                         and len(visible_paths(session, names)) == 10,
                         f"{mode} held-resize fixture did not finish scanning")
            session.key(b"n")
            settled_home(session, lambda: visible_paths(session, names) == names[:10],
                         f"{mode} held-resize fixture name order did not settle")

            def rows_ready(count):
                return (session.screen.normal_view() and not preview_rows(session)
                        and visible_paths(session, names) == names[:count])

            if mode == "visual":
                session.key(b"v")
                wait_visual(session, names, names[:1], cursor=names[0])
            samples.append(stationary_sample(
                session, lambda: rows_ready(10) and cursor_path(session, names) == names[0],
                f"{mode} list before held resize"))

            # Header occupies terminal row 1: main-list row 10 is SGR row 11.
            # Keep the button down, with no motion or keyboard input, across
            # both resizes. Shrinking may clip the captured cursor completely.
            session.key(b"\x1b[<0;35;11M")

            def held_ready(count, cursor):
                return (rows_ready(count) and cursor_path(session, names) == cursor
                        and (mode != "visual" or visual_count(session) == 2))

            samples.append(stationary_sample(
                session, lambda: held_ready(10, names[9]),
                f"{mode} bottom-row press before resize"))
            session.resize(7, 80)
            samples.append(stationary_sample(
                session, lambda: held_ready(6, None),
                f"{mode} held press clipped by shrink without input"))
            session.resize(11, 80)
            samples.append(stationary_sample(
                session, lambda: held_ready(10, names[9]),
                f"{mode} held press restored by growth without input"))

            if mode == "visual":
                # Drag back to an interior row after growth. The original
                # pressed entry is still the anchor, and the pre-press mark
                # at the first row must survive the added range.
                session.key(b"\x1b[<32;35;8M\x1b[<0;35;8m")
                expected = names[:1] + names[6:10]
                wait_visual(session, names, expected, cursor=names[6])
                samples.append(stationary_sample(
                    session, lambda: rows_ready(10)
                    and visual_count(session) == len(expected)
                    and visual_paths(session, names) == expected
                    and cursor_path(session, names) == names[6],
                    "visual original anchored range after held resize"))
            else:
                session.key(b"\x1b[<0;35;11m")
                samples.append(stationary_sample(
                    session, lambda: held_ready(10, names[9]),
                    "normal pressed row after held-resize release"))
            assert all((root / name).is_file() for name in names), \
                "held resize changed owned fixture files"
            elapsed = session.finish(key=b"q")
            metrics.append({"case": mode + "-held-mouse-resize",
                            "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": [], "idle": samples})
        finally:
            session.close()
    return metrics


def check_owned_resize_input(binary, work, threads):
    root = work / "input"
    root.mkdir()
    (root / "folder" / "nested").mkdir(parents=True)
    (root / "folder" / "nested" / "payload").write_bytes(os.urandom(65536))
    (root / "last-file").write_bytes(b"input")
    session = Session(binary, root, threads)
    try:
        prepare_input_fixture(session, root, "resize/input")
        stopped_resize_key(session, 28, 140, b"\x1b[F")
        session.wait(lambda: session.screen.help_at_edge()
                     and session.screen.selected_row() == 2
                     and session.screen.main_line(2).rstrip().endswith(" last-file")
                     and "Contents" not in session.screen.text(),
                     "simultaneous resize and End lost owned PTY input")
        elapsed = session.finish(key=b"q")
        return {"case": "simultaneous-resize-input", "first_frame_ms": session.first_frame_ms,
                "quit_ms": elapsed, "input": []}
    finally:
        session.close()


def queued_input_bytes(session):
    return struct.unpack("I", fcntl.ioctl(session.slave, termios.FIONREAD, struct.pack("I", 0)))[0]


def consumed_fragment(session, prefix, label):
    # A stopped child lets us first prove the bytes reached its input queue.
    with stopped_child(session):
        session.key(prefix)
        session.wait(lambda: queued_input_bytes(session) == len(prefix),
                     f"{label} did not reach owned child's input queue", timeout=5)
    session.wait(lambda: queued_input_bytes(session) == 0,
                 f"owned child did not consume {label}", timeout=5)


def check_owned_fragmented_input(binary, work, threads):
    root = work / "fragmented-input"
    root.mkdir()
    (root / "folder" / "nested").mkdir(parents=True)
    (root / "folder" / "nested" / "payload").write_bytes(os.urandom(65536))
    (root / "last-file").write_bytes(b"input")
    metrics = []
    for label, prefix, completion in (("utf8", b"\xc3", b"\xa9\x1b[F"),
                                      ("escape", b"\x1b[", b"F")):
        session = Session(binary, root, threads)
        try:
            prepare_input_fixture(session, root, "fragmented " + label)
            consumed_fragment(session, prefix, label)
            # Clearing the harness viewport requires a newly painted resized header.
            session.resize(28, 140)
            session.wait(lambda: path_in_header(session.screen, root),
                         f"incomplete {label} blocked resize repaint", timeout=5)
            assert session.child.poll() is None, f"incomplete {label} terminated owned child"
            session.key(completion)
            session.wait(lambda: session.screen.selected_row() == 2
                         and session.screen.main_line(2).rstrip().endswith(" last-file")
                         and "Contents" not in session.screen.text(),
                         f"completed {label} prevented End from being parsed", timeout=5)
            consumed_fragment(session, prefix, label)
            # Deliberately leave the last sequence incomplete through controlled exit.
            elapsed = session.finish(sig=signal.SIGTERM)
            metrics.append({"case": "fragmented-" + label,
                            "first_frame_ms": session.first_frame_ms,
                            "quit_ms": elapsed, "input": []})
        finally:
            session.close()
    return metrics


def has_multiple_entries(root):
    # The scanner treats a final symlink or file as a leaf; scandir only real roots.
    if root.is_symlink() or not root.is_dir():
        return False
    with os.scandir(root) as entries:
        return next(entries, None) is not None and next(entries, None) is not None

def check(binary, root, owned_fixture, max_input_ms, threads):
    metrics = []
    for quit_action in ("startup", "q", "ctrl-c", "sigterm", "missing-root"):
        session = Session(binary, root / "does-not-exist" if quit_action == "missing-root" else root, threads)
        inputs = []
        try:
            session.wait(lambda: b"Waiting to scan" in session.raw, "initial frame missing")
            assert session.raw.find(b"Waiting to scan") < session.raw.find(b"Scanning") or b"Scanning" not in session.raw
            if quit_action == "startup":
                # No input before this check: startup must follow the first sorted row.
                # The owned corpus has a known largest entry; its discovery order is irrelevant.
                session.wait(lambda: path_in_header(session.screen, root)
                             and session.screen.selected_row() == 1
                             and (not owned_fixture or first_row(session).rstrip().endswith(" large/")),
                             "untouched startup did not settle at the largest first row")
                elapsed = session.finish(key=b"q")
            elif quit_action == "q":
                session.wait(lambda: b"Scanning" in session.raw or path_in_header(session.screen, root),
                             "scan did not start or complete for the expected root")
                inputs.append(measured_key(session, b"?", lambda: session.screen.has_modal_title("Help"), "open help"))
                inputs.append(measured_key(session, b"\x1b", lambda: not session.screen.has_modal_title("Help") and session.screen.normal_view(), "close help"))
                session.wait(lambda: "Scan complete" in session.screen.text(header=True), "scan did not complete")
                # Measure actual visible selection feedback, rather than only opening overlays.
                if has_multiple_entries(root):
                    # Begin at the list boundary before measuring one-row moves.
                    session.key(b"\x1b[H")
                    session.wait(lambda: session.screen.selected_row() == 1
                                 and session.screen.main_line(2).strip(), "Home did not expose two rows")
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
                    session.wait(lambda: preview_fixture_ready(session),
                                 "wide hovered-directory preview did not show its entries")
                    check_preview_rows(session)
                    fixture_selection(session, "preview-file")
                    session.wait(lambda: "Preview" in preview_text(session)
                                 and "PTY text preview" in preview_text(session)
                                 and "second line" in preview_text(session),
                                 "text file did not render its content in the preview")
                    for name, kind in (("preview-pdf.pdf", "PDF"),
                                       ("preview-image.png", "Image"),
                                       ("preview-binary", "Binary")):
                        fixture_selection(session, name)
                        session.wait(lambda: kind in preview_text(session)
                                     and "Size" in preview_text(session)
                                     and "Disk" in preview_text(session)
                                     and not any(marker in preview_text(session) for marker in
                                                 ("PTY text preview", "PDF_SENTINEL", "BINARY_SENTINEL")),
                                     f"{kind} did not replace old text with file metadata")
                    loading = set()
                    session.observe = lambda screen: loading.update(
                        marker for marker in ("Loading", "Ordering entries", "Updating", "Waiting for scan")
                        if any(marker in row for row in preview_rows(session)))
                    try:
                        session.key(b"\x1b[H")
                        session.wait(lambda: preview_fixture_ready(session),
                                     "directory hover did not restore cached preview rows")
                    finally:
                        session.observe = None
                    assert not loading, f"hovering a cached directory flashed a placeholder: {loading}"
                    check_preview_rows(session)
                    assert "PTY text preview" not in preview_text(session), "directory retained file content"
                    for width in (96, 160):
                        session.resize(28, width)
                        session.wait(lambda: session.screen.help_at_edge()
                                     and session.screen.cells[1][width * 4 // 7] == "│"
                                     and preview_fixture_ready(session),
                                     f"preview did not use the proportional pane at width {width}")
                    session.resize(28, 95)
                    session.wait(lambda: session.screen.help_at_edge() and not preview_rows(session)
                                 and "Contents" not in session.screen.text(), "narrow terminal still showed preview")
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
            (root / "preview-file").write_bytes(b"PTY text preview\nsecond line\n")
            (root / "preview-pdf.pdf").write_bytes(b"%PDF-1.7\nPDF_SENTINEL\n")
            (root / "preview-image.png").write_bytes(bytes.fromhex(
                "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489"
                "0000000b49444154789c636000020000050001a5f645400000000049454e44ae426082"))
            (root / "preview-binary").write_bytes(b"\0BINARY_SENTINEL")
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
        if args.root is None:
            metrics.append(check_owned_queue(args.binary, Path(work), args.threads))
            metrics.append(check_owned_queue_resize(args.binary, Path(work), args.threads))
            metrics.append(check_owned_error_queue(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_helper_shutdown(args.binary, Path(work), args.threads))
            metrics.append(check_owned_special_previews(args.binary, Path(work), args.threads))
            metrics.append(check_owned_mouse_navigation(args.binary, Path(work), args.threads))
            metrics.append(check_owned_mouse_header(args.binary, Path(work), args.threads))
            metrics.append(check_owned_cancellation(args.binary, Path(work), args.threads))
            metrics.append(check_owned_quit_guard(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_quit_interrupts(args.binary, Path(work), args.threads))
            metrics.append(check_owned_quit_completion(args.binary, Path(work), args.threads))
            metrics.append(check_owned_quit_completion(
                args.binary, Path(work), args.threads, replacement_error=True))
            metrics.append(check_owned_refresh(args.binary, Path(work), args.threads))
            metrics.append(check_owned_refresh_after_quit(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_idle(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_leaf_root_idle(args.binary, Path(work), args.threads))
            metrics.append(check_owned_drag_modes(args.binary, Path(work), args.threads))
            metrics.append(check_owned_visual(args.binary, Path(work), args.threads))
            metrics.append(check_owned_input_modes(args.binary, Path(work), args.threads))
            metrics.append(check_owned_filter(args.binary, Path(work), args.threads))
            metrics.append(check_owned_batch_queue(args.binary, Path(work), args.threads))
            metrics.append(check_owned_completion_input(args.binary, Path(work), args.threads))
            metrics.append(check_owned_visual_ranges(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_held_resize(args.binary, Path(work), args.threads))
            metrics.append(check_owned_resize_input(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_resize_confirmation(args.binary, Path(work), args.threads))
            metrics.extend(check_owned_fragmented_input(args.binary, Path(work), args.threads))
    if args.report_latencies:
        print(json.dumps({"pty": metrics, "requested_threads": args.threads,
                          "uses_cli_default_threads": args.threads is None,
                          "max_first_frame_ms": max(m["first_frame_ms"] for m in metrics),
                          "max_quit_ms": max(m["quit_ms"] for m in metrics),
                          "max_input_ms": max(s["response_ms"] for m in metrics for s in m["input"]),
                          "input_cap_ms": args.max_input_ms}))
    else:
        checks = ["initial frame", "untouched startup selection", "help response", "navigation", "resize", "q", "Ctrl-C", "SIGTERM",
                  "scan error", "terminal restoration"]
        if args.root is None:
            checks += ["shared preview bars", "text and binary file previews", "responsive pane widths", "cached hover preview", "navigation during reordering", "modal scrolling", "tiny-terminal deletion refusal", "hidden names", "incremental fixture deletion", "paused FIFO queue", "queued full-path deletion errors", "mouse viewport scrolling and back control", "trailing header paths", "queue reorder/remove/overlap", "queued descendant markers", "deletion cancellation attempt", "guarded deletion quit", "repeated Ctrl-C/SIGINT shutdown", "automatic quit after confirmed deletion", "scoped refresh totals and hardlinks", "visual keyboard/mouse selection", "normal/visual input modes", "current-directory fuzzy filter", "visual delete aliases", "pending range deletion", "held edge drag", "visual batch deletion", "confirmed FIFO during batch review", "deletion completions during buffered input", "simultaneous resize and End", "simultaneous resize and confirmation", "fragmented UTF-8 and Escape input", "SIGTERM with incomplete input"]
        if args.root is None:
            checks.append("target-replacement error after terminal restoration")
            checks.append("stationary paused queue growth and shrink without input")
            checks.append("held normal and visual mouse rows across shrink and growth without input")
            checks.append("normal mouse click opens directories, visual click selects, both back-arrow cells activate")
            checks.append("spaced plain drag selection and Ctrl-drag deselection, reversal and press modifiers")
            checks.append("symlink destinations without following targets, installed archive/PDF helpers, unsupported file fallback")
            if sys.platform.startswith("linux"):
                checks.append("owned preview helpers and descendants stop on q, Ctrl-C and SIGTERM")
            checks.append("manual refresh restores deletion survivor reconciliation after cancelled quit")
            checks.append("stationary idle after scan, preview, filter, refresh, deletion and empty navigation")
            checks.append("stationary file, symlink and missing roots with empty visual mode and refresh input")
        if any(sample["during_scanning"] and "help" in sample["action"]
               for metric in metrics for sample in metric["input"]):
            checks.append("help during scanning")
        if any(metric.get("cancellation_observed", False) for metric in metrics):
            checks.append("active cancellation with surviving files")
        print("PTY checks passed: " + ", ".join(checks) + ".")


if __name__ == "__main__":
    main()
