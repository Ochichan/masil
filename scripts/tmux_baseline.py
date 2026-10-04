#!/usr/bin/env python3
"""Extract static tmux reference data from a tmux source tree."""

import argparse
import contextlib
import copy
import datetime
import hashlib
import io
import json
import re
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

LIST_FIELDS = ("commands", "ordinary_options", "hooks", "format_callback_variables",
               "copy_mode_commands", "control_notifications", "default_bindings",
               "mode_input_sources", "regression_shell_scripts")
# This is a fixed audit list, not data extracted from a tmux table.
MODE_INPUT_SOURCES = ("server-client.c", "key-string.c", "mode-tree.c", "window-tree.c",
                      "window-buffer.c", "window-client.c", "window-customize.c",
                      "window-clock.c", "window-switch.c", "menu.c", "cmd-display-menu.c",
                      "prompt.c", "status.c")
SHA_SUPPORT_FILES = {
    "cmd.c", "cmd-find.c", "cmd-parse.y", "configure.ac", "control.c", "format.c",
    "input-keys.c", "input.c", "key-bindings.c", "key-string.c", "menu.c", "mode-tree.c",
    "options-table.c", "prompt.c", "server-client.c", "status.c", "tmux-protocol.h",
    "tmux.1", "tmux.c", "tmux.h", "tty-keys.c", "tty-term.c", "window-buffer.c",
    "window-client.c", "window-clock.c", "window-copy.c", "window-customize.c",
    "window-switch.c", "window-tree.c",
}
DEFAULT_METADATA = {
    "schema_version": 1, "notice_file": "tmux-NOTICE.txt",
    "repository": "https://github.com/tmux/tmux", "captured_on": datetime.date.today().isoformat(),
    "validation": "Static source extraction only; no build, server, or runtime comparison.",
    "scope_note": ("Counts cover named source tables, not all tmux features. Runtime defaults "
                   "depend on build, environment, terminal, and configuration. Dynamic formats, "
                   "mode handlers, command flags and conditional behavior remain required."),
}

def fail(message):
    raise ValueError(message)

def line(text, position):
    return text.count("\n", 0, position) + 1

def fail_at(filename, text, position, message):
    fail(f"{filename}:{line(text, position)}: {message}")

def read(source, filename):
    path = Path(source) / filename
    if not path.is_file():
        fail(f"missing source file {path}")
    return path.read_text(encoding="utf-8")

def skip(text, position):
    """Skip one C comment or quoted literal."""
    if text.startswith("/*", position):
        end = text.find("*/", position + 2)
        if end < 0:
            fail("unterminated C comment")
        return end + 2
    if text.startswith("//", position):
        end = text.find("\n", position + 2)
        return len(text) if end < 0 else end + 1
    quote = text[position]
    if quote not in "\"'":
        return position + 1
    position += 1
    while position < len(text):
        if text[position] == "\\":
            position += 2
        elif text[position] == quote:
            return position + 1
        else:
            position += 1
    fail("unterminated C quoted literal")

def skip_space_and_comments(text, position, end=None):
    limit = len(text) if end is None else end
    while position < limit:
        if text[position].isspace():
            position += 1
        elif text.startswith(("/*", "//"), position):
            position = skip(text, position)
        else:
            break
    return position

def strip_c_comments(text):
    """Replace C comments with spaces while retaining source positions and lines."""
    result, position = list(text), 0
    while position < len(text):
        if text.startswith("/*", position):
            end = text.find("*/", position + 2)
            if end < 0:
                fail("unterminated C comment")
            for index in range(position, end + 2):
                if result[index] != "\n":
                    result[index] = " "
            position = end + 2
        elif text.startswith("//", position):
            end = text.find("\n", position + 2)
            end = len(text) if end < 0 else end
            for index in range(position, end):
                result[index] = " "
            position = end
        elif text[position] in "\"'":
            position = skip(text, position)
        else:
            position += 1
    return "".join(result)

def mask_c_literals(text):
    """Mask quoted literals without changing positions, for code-token matching."""
    result, position = list(text), 0
    while position < len(text):
        if text[position] in "\"'":
            end = skip(text, position)
            for index in range(position, end):
                if result[index] != "\n":
                    result[index] = " "
            position = end
        else:
            position += 1
    return "".join(result)

def brace(text, start):
    if text[start] != "{":
        fail("expected opening brace")
    depth, position = 0, start
    while position < len(text):
        if text.startswith(("/*", "//"), position) or text[position] in "\"'":
            position = skip(text, position)
            continue
        if text[position] == "{":
            depth += 1
        elif text[position] == "}":
            depth -= 1
            if not depth:
                return position
        position += 1
    fail("unmatched opening brace")

def array(text, name):
    match = re.search(r"\b" + re.escape(name) + r"\s*\[\s*\]\s*=\s*\{", text)
    if match is None:
        fail(f"could not find {name}[]")
    start = text.find("{", match.start(), match.end())
    return start, brace(text, start)

def blocks(text, start, end):
    """Return direct brace-delimited rows in an array initializer."""
    result, position = [], start + 1
    while position < end:
        if text.startswith(("/*", "//"), position) or text[position] in "\"'":
            position = skip(text, position)
        elif text[position] == "{":
            close = brace(text, position)
            result.append((position, close))
            position = close + 1
        else:
            position += 1
    return result


def c_string(text, position):
    if position >= len(text) or text[position] != '"':
        fail("expected C string literal")
    end = skip(text, position)
    raw, output, index = text[position + 1:end - 1], [], 0
    escapes = {"a": "\a", "b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t",
               "v": "\v", "\\": "\\", "\"": '"', "'": "'", "?": "?"}
    while index < len(raw):
        char = raw[index]
        if char != "\\":
            output.append(char); index += 1; continue
        index += 1
        if index == len(raw):
            fail("trailing C escape")
        char = raw[index]
        if char == "\n":
            index += 1
        elif char in escapes:
            output.append(escapes[char]); index += 1
        elif char in "01234567":
            finish = index
            while finish < len(raw) and finish < index + 3 and raw[finish] in "01234567":
                finish += 1
            output.append(chr(int(raw[index:finish], 8))); index = finish
        elif char == "x":
            finish = index + 1
            while finish < len(raw) and raw[finish] in "0123456789abcdefABCDEF":
                finish += 1
            if finish == index + 1:
                fail("invalid hexadecimal C escape")
            output.append(chr(int(raw[index + 1:finish], 16))); index = finish
        elif char in "uU":
            width = 4 if char == "u" else 8
            value = raw[index + 1:index + 1 + width]
            if len(value) != width or any(c not in "0123456789abcdefABCDEF" for c in value):
                fail("invalid Unicode C escape")
            output.append(chr(int(value, 16))); index += width + 1
        else:
            output.append(char); index += 1
    return "".join(output), end


def next_string(text, position, end=None):
    limit = len(text) if end is None else end
    while position < limit:
        if text.startswith(("/*", "//"), position) or text[position] == "'":
            position = skip(text, position)
        elif text[position] == '"':
            value, finish = c_string(text, position)
            return value, position, finish
        else:
            position += 1
    fail("could not find C string literal")


def field_string(block, name):
    match = re.search(r"\." + re.escape(name) + r"\s*=\s*", block)
    if match is None:
        fail(f"missing .{name} field")
    position = skip_space_and_comments(block, match.end())
    if position == len(block) or block[position] != '"':
        fail(f".{name} field is not a C string literal")
    return c_string(block, position)[0]


def field_expression(block, name):
    match = re.search(r"\." + re.escape(name) + r"\s*=\s*([^,]+)", block)
    if match is None:
        fail(f"missing .{name} field")
    expression = re.sub(r"\s+", "", match.group(1))
    if not expression:
        fail(f"empty .{name} field")
    return expression

def table_entries(text, start, end, filename):
    try:
        return initializers(text, start, end)
    except ValueError as error:
        fail_at(filename, text, start, str(error))


def nonempty_table_entries(text, start, end, filename):
    """Return table rows after discarding whitespace- and comment-only entries."""
    result = []
    for begin, finish in table_entries(text, start, end, filename):
        entry = strip_c_comments(text[begin:finish]).strip()
        if entry:
            result.append((begin, finish, row_start(text, begin, finish), entry))
    return result


def checked_table_entries(text, start, end, filename, table, is_terminator):
    """Reject table terminators that are followed by another non-empty row."""
    entries = nonempty_table_entries(text, start, end, filename)
    for index, (_, _, position, entry) in enumerate(entries):
        if is_terminator(entry) and index != len(entries) - 1:
            fail_at(filename, text, position,
                    f"{table} terminator must be the last non-empty entry")
    return entries


def empty_braces(entry):
    return re.fullmatch(r"\{\s*\}", entry) is not None

def null_name(entry, field):
    entry = mask_c_literals(entry)
    if field == "first":
        return re.match(r"\{\s*NULL\s*(?:,|\})", entry) is not None
    return re.search(r"\." + re.escape(field) + r"\s*=\s*NULL\b", entry) is not None

def first_string(entry):
    position = skip_space_and_comments(entry, 1)
    if position == len(entry) or entry[position] != '"':
        fail("first table field is not a C string literal")
    return c_string(entry, position)[0]

def row_value(text, begin, finish):
    return strip_c_comments(text[begin:finish]).strip()

def row_start(text, begin, finish):
    return skip_space_and_comments(text, begin, finish)

def parse_row(filename, text, position, parser):
    try:
        return parser()
    except ValueError as error:
        if re.match(r"^[^:\n]+:\d+:", str(error)):
            raise
        fail_at(filename, text, position, str(error))


def extract_commands(source):
    text = read(source, "cmd.c")
    start, end = array(text, "cmd_table")
    definitions = {}
    for path in sorted(Path(source).glob("cmd-*.c")):
        body = path.read_text(encoding="utf-8")
        for match in re.finditer(r"\bconst\s+struct\s+cmd_entry\s+([A-Za-z0-9_]+_entry)\s*=\s*\{", body):
            definitions[match.group(1)] = (path.name, body, match.start(), brace(body, body.find("{", match.start(), match.end())))
    rows = []
    entries = checked_table_entries(
        text, start, end, "cmd.c", "cmd_table",
        lambda entry: entry == "NULL" or empty_braces(entry))
    for begin, finish, position, entry in entries:
        if empty_braces(entry) or entry == "NULL":
            continue
        match = re.fullmatch(r"&([A-Za-z0-9_]+_entry)", entry)
        if match is None:
            fail_at("cmd.c", text, position, "could not parse cmd_table row")
        symbol = match.group(1)
        if symbol not in definitions:
            fail_at("cmd.c", text, position, f"cmd_table entry {symbol} has no cmd-*.c definition")
        filename, body, position, finish = definitions[symbol]
        entry = body[position:finish + 1]
        alias = re.search(r"\.alias\s*=\s*(NULL|\")", entry)
        args = re.search(r"\.args\s*=\s*\{\s*", entry)
        if alias is None or args is None:
            fail_at(filename, body, position, f"incomplete command definition {symbol}")
        def command_row():
            return {"name": field_string(entry, "name"),
                    "alias": None if alias.group(1) == "NULL" else field_string(entry, "alias"),
                    "args_template": next_string(entry, args.end())[0], "source": filename,
                    "line": line(body, position)}
        rows.append(parse_row(filename, body, position, command_row))
    return rows


def extract_options(source):
    text = read(source, "options-table.c")
    start, end = array(text, "options_table")
    stripped = strip_c_comments(text)
    kinds = {match.group(1) for match in re.finditer(
        r"(?m)^\s*#define\s+OPTIONS_TABLE_([A-Za-z0-9_]*HOOK)\s*\(", stripped)}
    if not kinds:
        fail("could not find OPTIONS_TABLE_*HOOK definitions")
    masked = mask_c_literals(stripped)
    pattern = r"\bOPTIONS_TABLE_([A-Za-z0-9_]*HOOK)\s*\("
    for match in re.finditer(pattern, masked):
        kind = match.group(1)
        if kind not in kinds:
            fail_at("options-table.c", text, match.start(),
                    f"unknown hook macro kind {kind}")
    options, hooks = [], []
    entries = checked_table_entries(
        text, start, end, "options-table.c", "options_table",
        lambda entry: empty_braces(entry) or null_name(entry, "name"))
    for begin, finish, position, entry in entries:
        if empty_braces(entry) or null_name(entry, "name"):
            continue
        if entry.startswith("{"):
            if not entry.endswith("}"):
                fail_at("options-table.c", text, position, "could not parse options_table row")
            def option_row():
                name = field_string(entry, "name")
                if not name:
                    fail("empty .name field")
                return {"name": name, "scope_expression": field_expression(entry, "scope"),
                        "type_expression": field_expression(entry, "type"), "source": "options-table.c",
                        "line": line(text, position)}
            options.append(parse_row("options-table.c", text, position, option_row))
            continue
        call = re.match(pattern, mask_c_literals(entry))
        if call is None:
            fail_at("options-table.c", text, position, "could not parse options_table row")
        kind = call.group(1)
        if kind not in kinds:
            fail_at("options-table.c", text, position, f"unknown hook macro kind {kind}")
        try:
            close = closing_parenthesis(entry, call.end() - 1)
        except ValueError as error:
            fail_at("options-table.c", text, position, str(error))
        trailing = skip_space_and_comments(entry, close + 1)
        if trailing < len(entry) and entry[trailing] == ",":
            trailing = skip_space_and_comments(entry, trailing + 1)
        if trailing != len(entry):
            fail_at("options-table.c", text, position,
                    "hook row must contain only the macro call and an optional trailing comma")
        argument = skip_space_and_comments(entry, call.end())
        if argument == len(entry) or entry[argument] != '"':
            fail_at("options-table.c", text, position, "hook macro has no string name")
        name = parse_row("options-table.c", text, position, lambda: c_string(entry, argument)[0])
        hooks.append({"name": "after-" + name if kind == "AFTER_HOOK" else name, "kind": kind,
                      "source": "options-table.c", "line": line(text, position)})
    return options, hooks


def simple_table(source, filename, table, field):
    text = read(source, filename)
    start, end = array(text, table)
    rows = []
    entries = checked_table_entries(
        text, start, end, filename, table,
        lambda entry: empty_braces(entry) or null_name(entry, field))
    for begin, finish, position, entry in entries:
        if empty_braces(entry) or null_name(entry, field):
            continue
        if not (entry.startswith("{") and entry.endswith("}")):
            fail_at(filename, text, position, f"could not parse {table} row")
        def table_row():
            name = first_string(entry) if field == "first" else field_string(entry, field)
            if not name:
                fail(f"empty {field} field")
            return {"name": name, "source": filename, "line": line(text, position)}
        rows.append(parse_row(filename, text, position, table_row))
    return rows


def notifications(source):
    text = read(source, "tmux.1")
    start = text.find(".Sh CONTROL MODE")
    if start < 0:
        fail("could not find CONTROL MODE section")
    end = text.find("\n.Sh ", start + 1)
    end = len(text) if end < 0 else end
    return [{"name": match.group(1).replace(r"\-", "-"), "source": "tmux.1", "line": line(text, start + match.start())}
            for match in re.finditer(r"(?m)^\.It(?: Xo)? Ic (%[A-Za-z0-9\\-]+)", text[start:end])]


def macros(text, filename):
    """Read multiline DEFAULT_* expressions, dropping C line continuations."""
    lines, result, index = text.splitlines(keepends=True), {}, 0
    offsets, offset = [], 0
    for source_line in lines:
        offsets.append(offset)
        offset += len(source_line)
    while index < len(lines):
        match = re.match(r"\s*#define\s+(DEFAULT_[A-Za-z0-9_]+)\b(.*)", lines[index])
        if match is None:
            index += 1; continue
        name, body, begin = match.group(1), match.group(2), index
        while body.rstrip("\r\n").rstrip().endswith("\\"):
            body = body.rstrip("\r\n").rstrip()[:-1]; index += 1
            if index == len(lines):
                fail(f"{filename}:{line(text, offsets[begin])}: unfinished macro {name}")
            body += "\n" + lines[index]
        result[name] = (body, line(text, offsets[begin])); index += 1
    return result


def expand(expression, definitions, active=(), filename="key-bindings.c", source_line=1):
    output, position = [], 0
    while position < len(expression):
        if expression.startswith(("/*", "//"), position):
            position = skip(expression, position)
        elif expression[position].isspace():
            position += 1
        elif expression[position] == '"':
            value, position = c_string(expression, position); output.append(value)
        elif re.match(r"[A-Za-z_]", expression[position:]):
            name = re.match(r"[A-Za-z_][A-Za-z0-9_]*", expression[position:]).group(0)
            name_position = position
            position += len(name)
            if not name.startswith("DEFAULT_") or name not in definitions:
                fail(f"{filename}:{source_line + expression.count(chr(10), 0, name_position)}: "
                     f"unexpected identifier {name} in default binding expression")
            if name in active:
                fail(f"{filename}:{source_line + expression.count(chr(10), 0, name_position)}: "
                     f"recursive default menu macro {name}")
            body, macro_line = definitions[name]
            output.append(expand(body, definitions, active + (name,), filename, macro_line))
        else:
            fail(f"{filename}:{source_line + expression.count(chr(10), 0, position)}: "
                 f"unexpected token {expression[position]!r} in default binding expression")
    return "".join(output)


def closing_parenthesis(text, start):
    if start >= len(text) or text[start] != "(":
        fail("expected opening parenthesis")
    depth, position = 0, start
    while position < len(text):
        if text.startswith(("/*", "//"), position) or text[position] in "\"'":
            position = skip(text, position)
            continue
        if text[position] == "(":
            depth += 1
        elif text[position] == ")":
            depth -= 1
            if not depth:
                return position
        position += 1
    fail("unmatched opening parenthesis")


def initializers(text, start, end):
    result, begin, position, stack = [], start + 1, start + 1, []
    closing = {")": "(", "}": "{", "]": "["}
    while position < end:
        if text.startswith(("/*", "//"), position) or text[position] in "\"'":
            position = skip(text, position)
        elif text[position] in "({[":
            stack.append(text[position]); position += 1
        elif text[position] in closing:
            if not stack or stack[-1] != closing[text[position]]:
                fail("mismatched delimiter in initializer")
            stack.pop(); position += 1
        elif text[position] == "," and not stack:
            result.append((begin, position)); begin = position + 1; position += 1
        else:
            position += 1
    if stack:
        fail("unclosed delimiter in initializer")
    return result + ([(begin, end)] if text[begin:end].strip() else [])


def word(text, position):
    while position < len(text) and text[position].isspace():
        position += 1
    if position == len(text):
        return None, position
    output = []
    while position < len(text) and not text[position].isspace():
        char = text[position]
        if char in "'\"":
            quote, position = char, position + 1
            while position < len(text) and text[position] != quote:
                if quote == '"' and text[position] == "\\" and position + 1 < len(text):
                    position += 1
                output.append(text[position]); position += 1
            if position == len(text):
                fail("unterminated tmux quoted word")
            position += 1
        elif char == "\\" and position + 1 < len(text):
            output.append(text[position + 1]); position += 2
        else:
            output.append(char); position += 1
    return "".join(output), position


def binding_fields(command, filename, source_line):
    def binding_fail(message):
        fail(f"{filename}:{source_line}: {message}")

    first, position = word(command, 0)
    if first not in {"bind", "bind-key"}:
        binding_fail(f"default initializer is not a bind command: {command[:60]}")
    table, repeat, note = "prefix", False, None
    # Parse the getopt-style bind short-option spec: nrN:T:.
    options = True
    while True:
        item, position = word(command, position)
        if item is None:
            binding_fail("bind command has no key")
        if options and item == "--":
            options = False
            continue
        if options and item.startswith("-") and item != "-":
            index = 1
            while index < len(item):
                flag = item[index]
                if flag not in "nrNT":
                    binding_fail(f"unknown bind flag -{flag}")
                if flag == "n":
                    table = "root"; index += 1
                elif flag == "r":
                    repeat = True; index += 1
                else:
                    if index + 1 < len(item):
                        value = item[index + 1:]
                    else:
                        value, position = word(command, position)
                        if value is None:
                            binding_fail(f"-{flag} has no value")
                    if flag == "N":
                        note = value
                    else:
                        table = value
                    break
            continue
        return table, item, repeat, note


def extract_bindings(source):
    text = read(source, "key-bindings.c")
    definitions = macros(text, "key-bindings.c")
    match = re.search(r"static\s+const\s+char\s*\*\s*const\s+defaults\s*\[\s*\]\s*=\s*\{", text)
    if match is None: fail("could not find key binding defaults[]")
    start = text.find("{", match.start(), match.end()); end = brace(text, start)
    rows = []
    for begin, finish in initializers(text, start, end):
        if not text[begin:finish].strip(): continue
        command = expand(text[begin:finish], definitions, filename="key-bindings.c",
                         source_line=line(text, begin))
        _, position, _ = parse_row("key-bindings.c", text, begin,
                                   lambda: next_string(text, begin, finish))
        table, key, repeat, note = parse_row(
            "key-bindings.c", text, position,
            lambda: binding_fields(command, "key-bindings.c", line(text, position)))
        rows.append({"table": table, "key": key, "repeat": repeat, "note": note,
                     "source": "key-bindings.c", "line": line(text, position), "command": command})
    init = re.search(r"(?m)^key_bindings_init\(void\)", text)
    if init is None: fail("could not find key_bindings_init")
    return rows, {name: expand(body, definitions, filename="key-bindings.c", source_line=macro_line)
                  for name, (body, macro_line) in definitions.items()}, line(text, init.start())


def version(source):
    match = re.search(r"AC_INIT\s*\(\s*\[tmux\]\s*,\s*([^\)]+)\)", read(source, "configure.ac"))
    if match is None: fail("could not find tmux AC_INIT version")
    return match.group(1).strip()


def source_commit(source):
    source = Path(source).resolve()
    top = subprocess.run(["git", "-C", str(source), "rev-parse", "--show-toplevel"], text=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if top.returncode:
        fail(f"--source {source} is not a git worktree root; provide --commit")
    top_level = top.stdout.strip()
    if not top_level or Path(top_level).resolve() != source:
        fail(f"--source {source} is not the git worktree root; provide --commit")
    resolved = subprocess.run(["git", "-C", str(source), "rev-parse", "HEAD"], text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sha = resolved.stdout.strip()
    if resolved.returncode or re.fullmatch(r"[0-9a-fA-F]{40}", sha) is None:
        fail(f"could not resolve --source {source} HEAD; provide --commit")
    status = subprocess.run(["git", "-C", str(source), "status", "--porcelain"], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if status.returncode:
        fail(f"could not inspect --source {source} status; provide --commit")
    if status.stdout:
        fail(f"--source {source} has uncommitted changes; provide --commit")
    return sha


def validate_commit(commit):
    if commit is not None and re.fullmatch(r"[0-9a-fA-F]{40}", commit) is None:
        fail("--commit must be 40 hexadecimal characters")
    return commit.lower() if commit is not None else None


def selected_commit(args, source, archived_commit, commit):
    if args.git and commit is not None:
        if archived_commit is None or commit.lower() != archived_commit.lower():
            fail(f"--commit {commit} does not match resolved --git SHA {archived_commit}")
        return archived_commit
    return commit or archived_commit or source_commit(source)


def hashes(source, commands, template):
    names = (sorted(template["source_sha256"]) if template and isinstance(template.get("source_sha256"), dict)
             else sorted(SHA_SUPPORT_FILES | {entry["source"] for entry in commands}))
    result = {}
    for name in names:
        path = Path(source) / name
        if not path.is_file(): fail(f"missing SHA-256 source file {path}")
        result[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def scripts(source):
    directory = Path(source) / "regress"
    if not directory.is_dir(): fail(f"missing regress directory {directory}")
    return [f"regress/{path.name}" for path in sorted(directory.glob("*.sh")) if path.is_file()]


def notice(source):
    named = []
    for filename in ("key-bindings.c", "options-table.c"):
        for text in read(source, filename).splitlines():
            if "Copyright" in text and "Nicholas Marriott" in text:
                text = text.strip(" */")
                if text not in named: named.append(text)
    if len(named) < 2: fail("could not find tmux copyright lines")
    return ["# ", *("# " + text for text in named), "# ",
            *("# " + text for text in read(source, "COPYING").splitlines())]


def conf(data, init_line, source):
    commands = [entry["command"] for entry in data["default_bindings"]]
    header = ["# tmux default binding source snapshot, not a masil implementation.",
              f"# Source: {data['commit']} / key-bindings.c:{init_line}",
              "# C string escapes decoded; DEFAULT_* menu macros expanded.",
              f"# {len(commands)} initializer entries, preserving original order.",
              f"# See {data['notice_file']} for original copyright and permission notices.", "",
              f"# Derived reference data from tmux {data['commit']}.", *notice(source), ""]
    return "\n".join(header + commands) + "\n"


def item_key(field, item):
    if field == "default_bindings": return item["table"], item["key"]
    return item["name"] if isinstance(item, dict) else item


def validate(data):
    for field in LIST_FIELDS:
        if not data[field]: fail(f"extracted list is empty: {field}")
        keys = [item_key(field, item) for item in data[field]]
        if len(keys) != len(set(keys)): fail(f"duplicate entries in {field}")
        if any(isinstance(item, dict) and item.get("line", 1) < 1 for item in data[field]):
            fail(f"invalid source line in {field}")


def snapshot(source, template=None, selected_commit=None, render_conf=True):
    source, template = Path(source), template or {}
    if render_conf and selected_commit is None:
        fail("missing tmux commit")
    commands = extract_commands(source); options, hooks = extract_options(source)
    formats = simple_table(source, "format.c", "format_table", "first")
    copy_commands = simple_table(source, "window-copy.c", "window_copy_cmd_table", "command")
    bindings, menus, init_line = extract_bindings(source); control = notifications(source); regress = scripts(source)
    missing = [name for name in MODE_INPUT_SOURCES if not (source / name).is_file()]
    if missing: fail("missing mode input source " + ", ".join(missing))
    metadata = {key: copy.deepcopy(template.get(key, value)) for key, value in DEFAULT_METADATA.items()}
    tables = {}
    for entry in bindings: tables[entry["table"]] = tables.get(entry["table"], 0) + 1
    data = {"schema_version": metadata["schema_version"], "notice_file": metadata["notice_file"],
            "repository": metadata["repository"], "commit": selected_commit,
            "source_version": version(source), "captured_on": metadata["captured_on"],
            "validation": metadata["validation"], "scope_note": metadata["scope_note"],
            "counts": {"commands": len(commands), "builtin_aliases": sum(x["alias"] is not None for x in commands),
                       "ordinary_options": len(options), "hooks": len(hooks), "format_callback_variables": len(formats),
                       "copy_mode_commands": len(copy_commands), "control_notifications": len(control),
                       "default_binding_initializers": len(bindings), "repeat_bindings": sum(x["repeat"] for x in bindings),
                       "regression_shell_scripts": len(regress)}, "binding_tables": tables,
            "source_sha256": hashes(source, commands, template), "commands": commands, "ordinary_options": options,
            "hooks": hooks, "format_callback_variables": formats, "copy_mode_commands": copy_commands,
            "control_notifications": control, "default_bindings": bindings, "expanded_default_menus": menus,
            "mode_input_sources": list(MODE_INPUT_SOURCES), "regression_shell_scripts": regress}
    validate(data)
    return data, conf(data, init_line, source) if render_conf else None


def changes(field, old, new):
    before = {item_key(field, item): item for item in old}; after = {item_key(field, item): item for item in new}
    added = [key for key in after if key not in before]; removed = [key for key in before if key not in after]
    changed = [key for key in before if key in after and before[key] != after[key]]
    order = not added and not removed and [item_key(field, x) for x in old] != [item_key(field, x) for x in new]
    return before, after, added, removed, changed, order


def show(value):
    return repr(value) if isinstance(value, tuple) else str(value)


def check_list(field, old, new):
    if field == "mode_input_sources":
        if old == new:
            print(f"mode_input_sources: fixed list, {len(new)} files present")
            return True
        added = [name for name in new if name not in old]
        removed = [name for name in old if name not in new]
        parts = (["added " + ", ".join(added)] if added else ["added none"])
        parts += (["removed " + ", ".join(removed)] if removed else ["removed none"])
        if not added and not removed:
            parts.append("order changed")
        print(f"mode_input_sources: fixed list, {len(new)} files present; " + "; ".join(parts))
        return False
    if old == new:
        print(f"{field}: same {len(old)}"); return True
    _, _, added, removed, changed, order = changes(field, old, new)
    parts = (["added " + ", ".join(map(show, added))] if added else []) + (["removed " + ", ".join(map(show, removed))] if removed else []) + (["changed " + ", ".join(map(show, changed))] if changed else []) + (["order changed"] if order else [])
    print(f"{field}: " + "; ".join(parts or ["items differ"])); return False


def check_map(field, old, new, no_changes=False):
    if old == new:
        print(f"{field}: {'no changes' if no_changes else f'same {len(old)}'}"); return True
    keys = sorted(key for key in set(old) | set(new) if old.get(key) != new.get(key))
    print(f"{field}: changed " + ", ".join(keys)); return False


def run_check(args):
    baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
    if "commit" not in baseline:
        fail(f"baseline {args.baseline} has no commit")
    commit = validate_commit(args.commit)
    with selected(args) as (source, archived_commit):
        commit = selected_commit(args, source, archived_commit, commit)
        actual, actual_conf = snapshot(source, baseline, commit)
    same = all([check_list(field, baseline.get(field, []), actual[field]) for field in LIST_FIELDS])
    same = all([check_map(field, baseline.get(field, {}), actual[field]) for field in ("expanded_default_menus", "counts", "binding_tables", "source_sha256")]) and same
    keep = set(LIST_FIELDS) | {"captured_on", "commit", "expanded_default_menus", "counts", "binding_tables", "source_sha256"}
    metadata_same = ({k: v for k, v in baseline.items() if k not in keep} ==
                     {k: v for k, v in actual.items() if k not in keep})
    commit_same = baseline["commit"] == actual["commit"]
    if not commit_same:
        print(f"metadata: differs commit {baseline['commit']} != {actual['commit']}")
        same = False
    if not metadata_same:
        print("metadata: changed")
        same = False
    if commit_same and metadata_same:
        print("metadata: same")
    expected, received = Path(args.conf).read_text(encoding="utf-8").splitlines(), actual_conf.splitlines()
    if expected == received: print(f"default_bindings.conf: same {len(received)}")
    else: print(f"default_bindings.conf: changed {sum(a != b for a, b in zip(expected, received))}; expected {len(expected)} lines, got {len(received)}"); same = False
    return 0 if same else 1


def diff_list(field, old, new):
    if field == "mode_input_sources":
        print(f"mode_input_sources: fixed list, {len(new)} files present")
        return old != new
    before, after, added, removed, changed, order = changes(field, old, new)
    lines, listed = 0, []
    for key in changed:
        left, right = before[key], after[key]
        if isinstance(left, dict) and {k: v for k, v in left.items() if k != "line"} == {k: v for k, v in right.items() if k != "line"}:
            lines += 1; continue
        if field == "commands": listed.append(f"{key} (alias {left['alias']!r}->{right['alias']!r}, args_template {left['args_template']!r}->{right['args_template']!r})")
        elif field == "ordinary_options": listed.append(f"{key} (scope {left['scope_expression']}->{right['scope_expression']}, type {left['type_expression']}->{right['type_expression']})")
        else: listed.append(show(key))
    if not (added or removed or listed or lines or order): print(f"{field}: no changes"); return False
    parts = (["added " + ", ".join(map(show, added))] if added else []) + (["removed " + ", ".join(map(show, removed))] if removed else []) + (["changed " + ", ".join(listed)] if listed else []) + ([f"{lines} line-number-only changes"] if lines else []) + (["order changed"] if order else [])
    print(f"{field}: " + "; ".join(parts)); return True


def run_diff(args):
    if (args.old or args.new) and (args.old_rev or args.new_rev):
        fail("--old-rev/--new-rev cannot be used with --old/--new")
    if args.git:
        if args.old or args.new:
            fail("--git cannot be used with --old/--new")
        if not args.old_rev or not args.new_rev: fail("--git diff needs --old-rev and --new-rev")
        with git_tree(args.git, args.old_rev) as (old_source, old_commit), git_tree(args.git, args.new_rev) as (new_source, new_commit):
            old, _ = snapshot(old_source, selected_commit=old_commit, render_conf=False)
            new, _ = snapshot(new_source, selected_commit=new_commit, render_conf=False)
    else:
        if not args.old or not args.new: fail("diff needs --old DIR and --new DIR")
        old, _ = snapshot(args.old, render_conf=False); new, _ = snapshot(args.new, render_conf=False)
    changed = any([diff_list(field, old[field], new[field]) for field in LIST_FIELDS])
    changed = any([not check_map(field, old[field], new[field], True) for field in ("expanded_default_menus", "counts", "binding_tables", "source_sha256")]) or changed
    if not changed: print("no changes")
    return 0


def safe_extract(archive, destination):
    root = destination.resolve()
    for member in archive.getmembers():
        target = (destination / member.name).resolve()
        if root not in target.parents and target != root: fail("git archive contains an unsafe path")
    archive.extractall(destination, filter="data")


@contextlib.contextmanager
def git_tree(repository, revision):
    resolved = subprocess.run(
        ["git", "-C", repository, "rev-parse", "--verify", "--end-of-options", revision + "^{commit}"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sha = resolved.stdout.strip()
    if resolved.returncode or re.fullmatch(r"[0-9a-fA-F]{40}", sha) is None:
        fail(resolved.stderr.strip() or f"could not resolve commit {revision}")
    archive = subprocess.run(["git", "-C", repository, "archive", "--format=tar", sha], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if archive.returncode: fail(archive.stderr.decode(errors="replace").strip() or f"could not archive {revision}")
    with tempfile.TemporaryDirectory(prefix="tmux-baseline-") as temporary:
        with tarfile.open(fileobj=io.BytesIO(archive.stdout)) as tar: safe_extract(tar, Path(temporary))
        yield Path(temporary), sha


@contextlib.contextmanager
def selected(args):
    if args.source:
        if args.rev:
            fail("--rev cannot be used with --source")
        yield Path(args.source), None
    elif args.git and args.rev:
        with git_tree(args.git, args.rev) as result: yield result
    else: fail("provide --source DIR or --git REPO --rev SHA")


def report_template_counts(data, template, allow_shrink):
    shrunk = []
    for field in LIST_FIELDS:
        expected = template.get(field, [])
        if not isinstance(expected, list):
            fail(f"template {field} is not a list")
        actual_count, template_count = len(data[field]), len(expected)
        print(f"{field}: {actual_count} (template {template_count})")
        if actual_count < template_count:
            shrunk.append(f"{field} {actual_count} < {template_count}")
    if shrunk and not allow_shrink:
        fail("extracted list shrank: " + ", ".join(shrunk) + "; use --allow-shrink")


def run_extract(args):
    template = json.loads(Path(args.template).read_text(encoding="utf-8")) if args.template else None
    commit = validate_commit(args.commit)
    with selected(args) as (source, archived_commit):
        commit = selected_commit(args, source, archived_commit, commit)
        data, text = snapshot(source, template, commit)
    if template is not None:
        report_template_counts(data, template, args.allow_shrink)
    Path(args.output).write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    if args.conf_output:
        Path(args.conf_output).write_text(text, encoding="utf-8")


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__); commands = parser.add_subparsers(dest="command", required=True)
    def source_options(command):
        group = command.add_mutually_exclusive_group(required=True); group.add_argument("--source", metavar="DIR"); group.add_argument("--git", metavar="REPO"); command.add_argument("--rev", metavar="SHA")
    extract = commands.add_parser("extract"); source_options(extract)
    extract.add_argument("--commit", metavar="SHA"); extract.add_argument("--template", metavar="JSON"); extract.add_argument("--output", required=True, metavar="FILE"); extract.add_argument("--conf-output", metavar="FILE"); extract.add_argument("--allow-shrink", action="store_true")
    check = commands.add_parser("check"); source_options(check)
    check.add_argument("--commit", metavar="SHA"); check.add_argument("--baseline", required=True, metavar="JSON"); check.add_argument("--conf", required=True, metavar="FILE")
    diff = commands.add_parser("diff")
    diff.add_argument("--old", metavar="DIR"); diff.add_argument("--new", metavar="DIR"); diff.add_argument("--git", metavar="REPO"); diff.add_argument("--old-rev", metavar="SHA"); diff.add_argument("--new-rev", metavar="SHA")
    return parser


def main():
    args = build_parser().parse_args()
    try:
        if args.command == "extract": run_extract(args); return 0
        return run_check(args) if args.command == "check" else run_diff(args)
    except (OSError, ValueError, subprocess.SubprocessError, tarfile.TarError) as error:
        print(f"tmux_baseline.py: {error}", file=sys.stderr); return 2


if __name__ == "__main__":
    raise SystemExit(main())
