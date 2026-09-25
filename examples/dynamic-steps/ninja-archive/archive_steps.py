#!/usr/bin/env python3
"""Tar archive extraction as a Ninja dyndep graph, and its rattler-build adapter.

build.ninja runs one subcommand per rule:

    scan       (scantar)    read the archive members, write the Ninja dyndep file
    extract    (untar)      unpack the regular files, write the stamp
    digest     (digest)     sha256sum-compatible checksums of the unpacked files
    stats      (stats)      byte, line and word counts of the unpacked files
    summarize  (summarize)  report of the checksums and counts

recipe.yaml runs the others:

    sample           write the deterministic sample archive
    export           run the scanner edges of build.ninja, load the dyndep files
                     they write and declare every other edge needed for the
                     default targets as a generated rattler-build step
    install-members  copy the unpacked files into $PREFIX
    install-files    copy files into $PREFIX

`scan` and `extract` accept the same archives: regular files and directories
with portable relative names, no two of which name the same path on a
case-insensitive file system. Links, devices, absolute names, `..` and names
Windows cannot store fail the build instead.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
from collections import Counter
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field

# Declaration files rattler-build reads after a build step succeeded.
MANIFEST_ENV = "RATTLER_BUILD_STEP_MANIFEST"
INPUTS_ENV = "RATTLER_BUILD_STEP_INPUTS"

# Modification time of every sample archive entry, so the archive is the same
# byte for byte on every run.
SAMPLE_MTIME = 1_700_000_000


class StepError(Exception):
    """A failure reported without a traceback."""


class NinjaError(StepError):
    """A Ninja or dyndep file this adapter cannot load."""


# Files --------------------------------------------------------------------


def write_bytes(path: str, data: bytes, atomic: bool = True) -> None:
    """Writes `data` to `path`, creating its directory. An atomic write goes
    through a temporary file next to `path`, so neither Ninja nor a later step
    ever sees a partially written file."""
    directory = os.path.dirname(path)
    if directory:
        os.makedirs(directory, exist_ok=True)
    target = path + ".tmp" if atomic else path
    with open(target, "wb") as file:
        file.write(data)
    if atomic:
        os.replace(target, path)


def write_text(path: str, text: str, atomic: bool = True) -> None:
    write_bytes(path, text.encode("utf-8"), atomic)


def write_json(path: str, value: object, atomic: bool = True) -> None:
    write_text(path, json.dumps(value, indent=2) + "\n", atomic)


def unique(items: Sequence[str]) -> list[str]:
    """`items` without repetitions, in their first order."""
    return list(dict.fromkeys(items))


def canonical_path(path: str) -> str:
    """Canonicalizes `path` the way Ninja does: `/` separators (Windows also
    accepts `\\`), no empty or `.` components, and `name/..` pairs removed."""
    if os.name == "nt":
        path = path.replace("\\", "/")
    components: list[str] = []
    for component in path.split("/"):
        if component in ("", "."):
            continue
        if component == ".." and components and components[-1] != "..":
            components.pop()
            continue
        components.append(component)
    canonical = "/".join(components)
    if path.startswith("/"):
        return "/" + canonical
    return canonical or "."


# Archive members ----------------------------------------------------------

# Names Windows reserves for devices, in every directory and with any extension.
RESERVED_NAMES = frozenset(
    ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
    + [f"COM{number}" for number in range(1, 10)]
    + [f"LPT{number}" for number in range(1, 10)]
)
# Characters Windows does not allow in names. `\` also separates components
# there, `:` names alternate data streams, and Ninja cannot spell `|` in paths.
UNPORTABLE_CHARACTERS = frozenset('<>:"|?*\\')
# A DOS 8.3 short name: `~` and digits end the name before its extension, and
# it may name another file or directory under its short alias.
SHORT_NAME = re.compile(r"~[0-9]+\Z")


@dataclass(frozen=True)
class Member:
    """A regular file in the archive."""

    path: str
    """`/`-separated path, relative to the extraction directory."""
    info: tarfile.TarInfo


def member_path(name: str) -> str:
    """Returns the normalized path of archive member `name`: its components
    without empty and `.` ones, joined by `/`. Fails for names that escape the
    extraction directory or that another platform cannot store."""
    for char in name:
        if ord(char) < 0x20 or ord(char) == 0x7F:
            raise StepError(f"archive member {name!r}: control character in the name")
        if char in UNPORTABLE_CHARACTERS:
            raise StepError(
                f"archive member {name!r}: {char!r} is not portable in file names"
            )
    if name.startswith("/"):
        raise StepError(f"archive member {name!r}: absolute path")
    components = []
    for component in name.split("/"):
        if component in ("", "."):
            continue
        if component == "..":
            raise StepError(
                f"archive member {name!r}: '..' escapes the extraction directory"
            )
        if component.endswith((".", " ")):
            raise StepError(
                f"archive member {name!r}: Windows drops the dot or space ending {component!r}"
            )
        if component.split(".", 1)[0].rstrip(" ").upper() in RESERVED_NAMES:
            raise StepError(
                f"archive member {name!r}: {component!r} names a Windows device"
            )
        if SHORT_NAME.search(component.rsplit(".", 1)[0]):
            raise StepError(
                f"archive member {name!r}: {component!r} is spelled like a DOS short name"
            )
        components.append(component)
    return "/".join(components)


def open_archive(path: str) -> tarfile.TarFile:
    """Opens the tar archive at `path`, uncompressed or compressed with gzip,
    bzip2 or xz."""
    try:
        return tarfile.open(path, "r:*")
    except tarfile.TarError as error:
        raise StepError(f"{path}: not a tar archive ({error})") from error


def read_members(tar: tarfile.TarFile) -> list[Member]:
    """Validates every member of `tar` and returns its regular files, sorted by
    path. Directories are only validated: extraction creates the directories
    of the files, and no empty ones."""
    # Every file and directory by its case-folded path, as (kind, path).
    kinds: dict[str, tuple[str, str]] = {}
    files: list[Member] = []
    for info in tar.getmembers():
        if info.isfile():
            kind = "file"
        elif info.isdir():
            kind = "directory"
        elif info.issym() or info.islnk():
            raise StepError(
                f"archive member {info.name!r}: links are not extracted, "
                "as they can point outside the extraction directory"
            )
        else:
            raise StepError(
                f"archive member {info.name!r}: neither a regular file nor a directory"
            )
        path = member_path(info.name)
        if not path:
            if kind == "file":
                raise StepError(f"archive member {info.name!r}: a file without a name")
            continue
        key = path.casefold()
        previous = kinds.get(key)
        if previous is not None:
            raise StepError(
                f"archive member {info.name!r}: duplicates the {previous[0]} {previous[1]!r} "
                "(names are compared case-insensitively)"
            )
        kinds[key] = (kind, path)
        if kind == "file":
            files.append(Member(path, info))
    for key, (_, path) in kinds.items():
        components = key.split("/")
        for depth in range(1, len(components)):
            ancestor = kinds.get("/".join(components[:depth]))
            if ancestor is not None and ancestor[0] == "file":
                raise StepError(
                    f"archive member {path!r}: lies inside the file {ancestor[1]!r}"
                )
    files.sort(key=lambda member: member.path)
    return files


def extracted_path(dest: str, member: str) -> str:
    """The Ninja path of `member` unpacked into `dest`."""
    dest = canonical_path(dest)
    return member if dest == "." else dest.rstrip("/") + "/" + member


def member_file(dest: str, member: str) -> str:
    """The file system path of `member` unpacked into `dest`."""
    return os.path.join(dest, *member.split("/"))


def ensure_directory(path: str) -> None:
    if os.path.islink(path):
        raise StepError(f"{path}: symbolic link; refusing to extract through it")
    if os.path.isdir(path):
        return
    if os.path.lexists(path):
        raise StepError(f"{path}: exists and is not a directory")
    os.makedirs(path)


def prepare_file(dest: str, member: str) -> str:
    """Creates the directories of `member` below `dest`, removes whatever an
    earlier extraction left at its place without following symbolic links, and
    returns the path to write the member to."""
    directory = dest
    ensure_directory(directory)
    components = member.split("/")
    for component in components[:-1]:
        directory = os.path.join(directory, component)
        ensure_directory(directory)
    target = os.path.join(directory, components[-1])
    if os.path.islink(target) or os.path.isfile(target):
        os.unlink(target)
    elif os.path.lexists(target):
        raise StepError(f"{target}: exists and is not a file")
    return target


def read_stamp(path: str) -> tuple[str, list[str]]:
    """Returns the extraction directory and the members in the stamp that
    `extract` wrote to `path`."""
    with open(path, encoding="utf-8") as file:
        stamp = json.load(file)
    if not isinstance(stamp, dict) or stamp.get("version") != 1:
        raise StepError(f"{path}: not a version 1 stamp written by `extract`")
    dest = stamp.get("dest")
    members = stamp.get("members")
    if not isinstance(dest, str) or not isinstance(members, list):
        raise StepError(f"{path}: malformed stamp")
    for member in members:
        if not isinstance(member, str) or not member or member_path(member) != member:
            raise StepError(f"{path}: malformed member {member!r}")
    return dest, members


# Ninja files --------------------------------------------------------------

IDENTIFIER = re.compile(r"[A-Za-z0-9_.-]+")
SIMPLE_VARIABLE = re.compile(r"[A-Za-z0-9_-]+")
BRACED_VARIABLE = re.compile(r"\{([A-Za-z0-9_.-]+)\}")
LET = re.compile(r"([A-Za-z0-9_.-]+) *= *")
# Characters Ninja leaves unquoted in `$in` and `$out` on POSIX.
POSIX_SHELL_SAFE = re.compile(r"[A-Za-z0-9_+./-]+")
# Characters cmd.exe, which runs the generated steps on Windows, interprets
# outside double quotes: command separators, pipes, redirections and `^`.
# Ninja runs commands with CreateProcess there, which interprets none of them.
CMD_OPERATORS = frozenset("&|<>^")
# Characters for which `shell_quote` quotes a path on Windows: the space and
# `"`, which Ninja quotes for CreateProcess, and the characters cmd.exe
# interprets. Quoting them leaves the arguments CreateProcess passes the same.
WIN32_QUOTED = frozenset(' "()') | CMD_OPERATORS

# Rule variables this adapter evaluates or can ignore, and those it rejects
# because it would have to implement them to run the edges the way Ninja does.
RULE_VARIABLES = frozenset(["command", "description", "dyndep", "restat"])
UNSUPPORTED_RULE_VARIABLES = frozenset(
    [
        "depfile",
        "deps",
        "msvc_deps_prefix",
        "rspfile",
        "rspfile_content",
        "pool",
        "generator",
    ]
)

Lookup = Callable[[str], str]


def no_variable(name: str) -> str:
    """The scope of dyndep files, which only define their version."""
    return ""


class EvalString:
    """A Ninja string: literal text and `$variable` references."""

    def __init__(self, parts: list[tuple[bool, str]]) -> None:
        self.parts = parts

    def evaluate(self, lookup: Lookup) -> str:
        return "".join(
            lookup(text) if variable else text for variable, text in self.parts
        )

    def literal(self) -> str | None:
        if any(variable for variable, _ in self.parts):
            return None
        return "".join(text for _, text in self.parts)


def read_eval(text: str, pos: int, path: bool, where: str) -> tuple[EvalString, int]:
    """Reads a Ninja string from `text` at `pos`, up to the end of the line or,
    for a path, up to an unescaped space, `:` or `|`."""
    parts: list[tuple[bool, str]] = []
    literal: list[str] = []

    def flush() -> None:
        if literal:
            parts.append((False, "".join(literal)))
            literal.clear()

    while pos < len(text):
        char = text[pos]
        if path and char in " :|":
            break
        if char != "$":
            literal.append(char)
            pos += 1
            continue
        following = text[pos + 1 : pos + 2]
        if following in ("$", " ", ":"):
            literal.append(following)
            pos += 2
            continue
        match = BRACED_VARIABLE.match(text, pos + 1) or SIMPLE_VARIABLE.match(
            text, pos + 1
        )
        if match is None:
            raise NinjaError(f"{where}: bad $-escape (write a literal $ as $$)")
        flush()
        parts.append((True, match.group(match.lastindex or 0)))
        pos = match.end()
    flush()
    return EvalString(parts), pos


@dataclass
class Line:
    """A logical line: physical lines joined at `$` line continuations."""

    number: int
    indented: bool
    text: str


def logical_lines(text: str, filename: str) -> list[Line]:
    """Splits a Ninja file into logical lines without comments. A blank line
    is kept, with empty text, as it ends the bindings of a statement."""
    physical = text.split("\n")
    lines: list[Line] = []
    index = 0
    while index < len(physical):
        number = index + 1
        line = physical[index].rstrip("\r")
        index += 1
        content = line.lstrip(" ")
        if content.startswith("\t"):
            raise NinjaError(f"{filename}:{number}: tabs are not allowed, use spaces")
        if not content:
            lines.append(Line(number, False, ""))
            continue
        if content.startswith("#"):
            continue
        while (len(line) - len(line.rstrip("$"))) % 2 == 1:
            if index == len(physical):
                raise NinjaError(
                    f"{filename}:{number}: unexpected end of file after '$'"
                )
            line = line[:-1] + physical[index].rstrip("\r").lstrip(" ")
            index += 1
        lines.append(Line(number, line.startswith(" "), line.lstrip(" ")))
    return lines


def statements(lines: list[Line], filename: str) -> Iterator[tuple[Line, list[Line]]]:
    """Yields every top-level line with the indented binding lines after it."""
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if not line.text:
            continue
        if line.indented:
            raise NinjaError(f"{filename}:{line.number}: unexpected indent")
        block = []
        while index < len(lines) and lines[index].indented:
            block.append(lines[index])
            index += 1
        yield line, block


def parse_let(text: str, where: str) -> tuple[str, EvalString]:
    match = LET.match(text)
    if match is None:
        raise NinjaError(f"{where}: expected 'name = value', got {text!r}")
    value, _ = read_eval(text, match.end(), path=False, where=where)
    return match.group(1), value


def build_tokens(text: str, where: str) -> list[str | EvalString]:
    """Splits the part of a `build` or `default` line after the keyword into
    paths and the separators `:`, `|`, `||` and `|@`."""
    tokens: list[str | EvalString] = []
    pos = 0
    while True:
        while pos < len(text) and text[pos] == " ":
            pos += 1
        if pos == len(text):
            return tokens
        for separator in ("||", "|@", "|", ":"):
            if text.startswith(separator, pos):
                tokens.append(separator)
                pos += len(separator)
                break
        else:
            value, pos = read_eval(text, pos, path=True, where=where)
            tokens.append(value)


# The section of a build statement each separator starts, after a section.
NEXT_SECTION = {
    ("outputs", "|"): "implicit_outputs",
    ("outputs", ":"): "rule",
    ("implicit_outputs", ":"): "rule",
    ("inputs", "|"): "implicit_inputs",
    ("inputs", "||"): "order_only",
    ("implicit_inputs", "||"): "order_only",
}


def parse_statement(text: str, where: str) -> tuple[str, dict[str, list[EvalString]]]:
    """Parses `outs | implicit outs : rule ins | implicit ins || order-only`
    into the rule name and the paths of each section."""
    sections: dict[str, list[EvalString]] = {
        name: []
        for name in (
            "outputs",
            "implicit_outputs",
            "inputs",
            "implicit_inputs",
            "order_only",
        )
    }
    rule = ""
    section = "outputs"
    for token in build_tokens(text, where):
        if isinstance(token, EvalString):
            if section == "rule":
                name = token.literal()
                if name is None or not IDENTIFIER.fullmatch(name):
                    raise NinjaError(f"{where}: expected a rule name after ':'")
                rule = name
                section = "inputs"
            else:
                sections[section].append(token)
        elif token == "|@":
            raise NinjaError(
                f"{where}: validations ('|@') are not supported by this adapter"
            )
        elif (section, token) in NEXT_SECTION:
            section = NEXT_SECTION[(section, token)]
        else:
            raise NinjaError(f"{where}: unexpected '{token}'")
    if not rule:
        raise NinjaError(f"{where}: expected ': <rule>'")
    if not sections["outputs"]:
        raise NinjaError(f"{where}: expected an output")
    return rule, sections


def statement_path(value: EvalString, scope: Lookup, where: str) -> str:
    path = value.evaluate(scope)
    if not path:
        raise NinjaError(f"{where}: empty path")
    return canonical_path(path)


@dataclass(eq=False)
class Rule:
    name: str
    bindings: dict[str, EvalString]


@dataclass(eq=False)
class Edge:
    """A build statement, with the paths its dyndep file adds."""

    location: str
    rule: Rule
    outputs: list[str]
    implicit_outputs: list[str]
    inputs: list[str]
    implicit_inputs: list[str]
    order_only: list[str]
    bindings: dict[str, str]
    dyndep: str | None = None
    discovered_outputs: list[str] = field(default_factory=list)
    discovered_inputs: list[str] = field(default_factory=list)

    def all_outputs(self) -> list[str]:
        return self.outputs + self.implicit_outputs + self.discovered_outputs

    def all_inputs(self) -> list[str]:
        return (
            self.inputs
            + self.implicit_inputs
            + self.discovered_inputs
            + self.order_only
        )


class NinjaFile:
    """What this adapter understands of a Ninja file: top-level variables,
    rules, build statements and `default` targets."""

    def __init__(self, path: str) -> None:
        self.path = path
        self.variables: dict[str, str] = {}
        self.rules: dict[str, Rule] = {}
        self.edges: list[Edge] = []
        self.defaults: list[str] = []
        self.producers: dict[str, Edge] = {}

    def variable(self, name: str) -> str:
        return self.variables.get(name, "")

    def add_output(self, path: str, edge: Edge, where: str) -> None:
        if path in self.producers:
            raise NinjaError(f"{where}: multiple rules generate {path}")
        self.producers[path] = edge


def edge_variable(
    ninja: NinjaFile, edge: Edge, name: str, escape: bool, stack: tuple[str, ...] = ()
) -> str:
    """Looks up `name` for `edge` the way Ninja evaluates rule variables:
    `$in` and `$out` (quoted for the shell if `escape` is set), then the
    bindings of the build statement, then those of its rule, evaluated for
    the edge, then the top-level variables."""
    if name in ("in", "in_newline"):
        paths = [shell_quote(path) if escape else path for path in edge.inputs]
        return (" " if name == "in" else "\n").join(paths)
    if name == "out":
        return " ".join(shell_quote(path) if escape else path for path in edge.outputs)
    if name in edge.bindings:
        return edge.bindings[name]
    value = edge.rule.bindings.get(name)
    if value is None:
        return ninja.variable(name)
    if name in stack:
        raise NinjaError(
            f"{edge.location}: cycle in rule variables: {' -> '.join(stack + (name,))}"
        )
    return value.evaluate(
        lambda inner: edge_variable(ninja, edge, inner, escape, stack + (name,))
    )


def shell_quote(path: str) -> str:
    """Quotes `path` for a command line the way Ninja quotes `$in` and `$out`.
    On Windows, a path is also quoted if cmd.exe would interpret a character
    of it, which gives the program the same argument either way."""
    if os.name == "nt":
        if not WIN32_QUOTED.intersection(path):
            return path
        quoted = ['"']
        backslashes = 0
        for char in path:
            if char == "\\":
                backslashes += 1
            elif char == '"':
                quoted.append("\\" * (backslashes + 1))
                backslashes = 0
            else:
                backslashes = 0
            quoted.append(char)
        quoted.append("\\" * backslashes + '"')
        return "".join(quoted)
    if POSIX_SHELL_SAFE.fullmatch(path):
        return path
    return "'" + path.replace("'", "'\\''") + "'"


def cmd_exe_problem(command: str) -> str | None:
    """Why cmd.exe, which runs the generated steps on Windows, would not run
    `command` as the single program invocation Ninja's CreateProcess runs,
    or None if it would. cmd.exe expands `%` everywhere, ends a command at a
    line break, and interprets its operators outside double quotes, toggling
    at every `"`."""
    if "%" in command:
        return (
            "cmd.exe, which runs the steps on Windows, would expand '%' in the command"
        )
    if "\n" in command or "\r" in command:
        return "cmd.exe, which runs the steps on Windows, would end the command at a line break"
    quoted = False
    for char in command:
        if char == '"':
            quoted = not quoted
        elif char in CMD_OPERATORS and not quoted:
            return (
                f"cmd.exe, which runs the steps on Windows, would interpret '{char}' "
                "outside double quotes in the command"
            )
    return None


def parse_rule(ninja: NinjaFile, rest: str, block: list[Line], where: str) -> None:
    name = rest.strip(" ")
    if not IDENTIFIER.fullmatch(name):
        raise NinjaError(f"{where}: expected a rule name")
    if name == "phony" or name in ninja.rules:
        raise NinjaError(f"{where}: duplicate rule '{name}'")
    bindings: dict[str, EvalString] = {}
    for line in block:
        line_where = f"{ninja.path}:{line.number}"
        key, value = parse_let(line.text, line_where)
        if key in UNSUPPORTED_RULE_VARIABLES:
            raise NinjaError(
                f"{line_where}: rule variable '{key}' is not supported by this adapter"
            )
        if key not in RULE_VARIABLES:
            raise NinjaError(f"{line_where}: unexpected variable '{key}'")
        bindings[key] = value
    if "command" not in bindings:
        raise NinjaError(f"{where}: rule '{name}' has no command")
    ninja.rules[name] = Rule(name, bindings)


def parse_edge(ninja: NinjaFile, rest: str, block: list[Line], where: str) -> None:
    rule_name, sections = parse_statement(rest, where)
    if rule_name == "phony":
        raise NinjaError(f"{where}: phony edges are not supported by this adapter")
    rule = ninja.rules.get(rule_name)
    if rule is None:
        raise NinjaError(f"{where}: unknown build rule '{rule_name}'")
    bindings: dict[str, str] = {}
    for line in block:
        line_where = f"{ninja.path}:{line.number}"
        key, value = parse_let(line.text, line_where)
        if key in UNSUPPORTED_RULE_VARIABLES:
            raise NinjaError(
                f"{line_where}: variable '{key}' is not supported by this adapter"
            )
        # Like Ninja, in the top-level scope, when the statement is read.
        bindings[key] = value.evaluate(ninja.variable)

    def scope(name: str) -> str:
        return bindings[name] if name in bindings else ninja.variable(name)

    def paths(section: str) -> list[str]:
        return [statement_path(value, scope, where) for value in sections[section]]

    edge = Edge(
        location=where,
        rule=rule,
        outputs=paths("outputs"),
        implicit_outputs=paths("implicit_outputs"),
        inputs=paths("inputs"),
        implicit_inputs=paths("implicit_inputs"),
        order_only=paths("order_only"),
        bindings=bindings,
    )
    for output in edge.all_outputs():
        ninja.add_output(output, edge, where)
    dyndep = edge_variable(ninja, edge, "dyndep", escape=False)
    if dyndep:
        edge.dyndep = canonical_path(dyndep)
        if edge.dyndep not in edge.all_inputs():
            raise NinjaError(f"{where}: dyndep '{edge.dyndep}' is not an input")
    ninja.edges.append(edge)


def parse_ninja(path: str, overrides: dict[str, str]) -> NinjaFile:
    """Reads the Ninja file at `path`, with the top-level variables named in
    `overrides` bound to the given values instead of theirs."""
    with open(path, encoding="utf-8", newline="") as file:
        lines = logical_lines(file.read(), path)
    ninja = NinjaFile(path)
    unused = set(overrides)
    for line, block in statements(lines, path):
        where = f"{path}:{line.number}"
        keyword, _, rest = line.text.partition(" ")
        if keyword == "rule":
            parse_rule(ninja, rest, block, where)
            continue
        if keyword == "build":
            parse_edge(ninja, rest, block, where)
            continue
        if keyword in ("pool", "include", "subninja"):
            raise NinjaError(f"{where}: '{keyword}' is not supported by this adapter")
        if block:
            raise NinjaError(f"{path}:{block[0].number}: unexpected indent")
        if keyword == "default":
            for token in build_tokens(rest, where):
                if not isinstance(token, EvalString):
                    raise NinjaError(f"{where}: unexpected '{token}'")
                ninja.defaults.append(statement_path(token, ninja.variable, where))
            continue
        name, value = parse_let(line.text, where)
        # Ninja falls back to top-level variables for the rule variables of
        # every edge, so these would apply to all of them.
        if name in UNSUPPORTED_RULE_VARIABLES:
            raise NinjaError(
                f"{where}: variable '{name}' is not supported by this adapter"
            )
        if name in overrides:
            ninja.variables[name] = overrides[name]
            unused.discard(name)
        else:
            ninja.variables[name] = value.evaluate(ninja.variable)
    if unused:
        raise NinjaError(
            f"{path}: no top-level variable {', '.join(sorted(unused))} to --set"
        )
    for target in ninja.defaults:
        if target not in ninja.producers:
            raise NinjaError(
                f"{path}: no build statement produces the default target '{target}'"
            )
    return ninja


@dataclass
class DyndepStatement:
    location: str
    implicit_outputs: list[str]
    implicit_inputs: list[str]


def parse_dyndep(path: str) -> dict[str, DyndepStatement]:
    """Reads a dyndep file: `ninja_dyndep_version = 1`, then statements
    `build out | implicit outs : dyndep | implicit ins`, each with an optional
    `restat` binding, by their explicit output."""
    with open(path, encoding="utf-8", newline="") as file:
        lines = logical_lines(file.read(), path)
    found: dict[str, DyndepStatement] = {}
    versioned = False
    for line, block in statements(lines, path):
        where = f"{path}:{line.number}"
        if not versioned:
            match = LET.match(line.text)
            if match is None or match.group(1) != "ninja_dyndep_version":
                raise NinjaError(f"{where}: expected 'ninja_dyndep_version = 1'")
            _, value = parse_let(line.text, where)
            version = re.match(r"([0-9]+)(?:\.([0-9]+))?", value.evaluate(no_variable))
            if (
                version is None
                or int(version.group(1)) != 1
                or int(version.group(2) or 0) != 0
            ):
                raise NinjaError(f"{where}: unsupported dyndep version")
            if block:
                raise NinjaError(f"{path}:{block[0].number}: unexpected indent")
            versioned = True
            continue
        keyword, _, rest = line.text.partition(" ")
        if keyword != "build":
            raise NinjaError(f"{where}: expected a 'build' statement")
        rule, sections = parse_statement(rest, where)
        if rule != "dyndep":
            raise NinjaError(f"{where}: expected build command name 'dyndep'")
        if len(sections["outputs"]) != 1:
            raise NinjaError(f"{where}: expected exactly one explicit output")
        if sections["inputs"]:
            raise NinjaError(f"{where}: explicit inputs not supported")
        if sections["order_only"]:
            raise NinjaError(f"{where}: order-only inputs not supported")
        if len(block) > 1:
            raise NinjaError(f"{path}:{block[1].number}: unexpected indent")
        for binding in block:
            key, _ = parse_let(binding.text, f"{path}:{binding.number}")
            if key != "restat":
                raise NinjaError(f"{path}:{binding.number}: binding is not 'restat'")
        output = statement_path(sections["outputs"][0], no_variable, where)
        if output in found:
            raise NinjaError(f"{where}: multiple statements for '{output}'")
        found[output] = DyndepStatement(
            where,
            [
                statement_path(value, no_variable, where)
                for value in sections["implicit_outputs"]
            ],
            [
                statement_path(value, no_variable, where)
                for value in sections["implicit_inputs"]
            ],
        )
    if not versioned:
        raise NinjaError(f"{path}: expected 'ninja_dyndep_version = 1'")
    return found


def apply_dyndep(
    ninja: NinjaFile, path: str, found: dict[str, DyndepStatement]
) -> None:
    """Adds the implicit outputs and inputs in the dyndep file `path` to the
    edges bound to it, checking what Ninja checks when it loads the file."""
    updated: set[Edge] = set()
    for output, statement in found.items():
        edge = ninja.producers.get(output)
        if edge is None:
            raise NinjaError(
                f"{statement.location}: no build statement exists for '{output}'"
            )
        if edge.dyndep != path:
            raise NinjaError(
                f"{statement.location}: the build statement of '{output}' has no "
                f"dyndep binding for '{path}'"
            )
        if edge in updated:
            raise NinjaError(
                f"{statement.location}: multiple statements for the edge of '{output}'"
            )
        updated.add(edge)
        for implicit in statement.implicit_outputs:
            ninja.add_output(implicit, edge, statement.location)
        edge.discovered_outputs.extend(statement.implicit_outputs)
        edge.discovered_inputs.extend(statement.implicit_inputs)
    for edge in ninja.edges:
        if edge.dyndep == path and edge not in updated:
            raise NinjaError(
                f"{edge.location}: '{edge.outputs[0]}' not mentioned in its dyndep file '{path}'"
            )


def ninja_escape(path: str) -> str:
    """Spells `path` in a Ninja file."""
    return path.replace("$", "$$").replace(" ", "$ ").replace(":", "$:")


def dyndep_statement(
    output: str,
    implicit_outputs: Sequence[str],
    implicit_inputs: Sequence[str],
    restat: bool,
) -> str:
    """A dyndep file statement `build out | implicit outs : dyndep | implicit
    ins`, one path per continued line."""
    lines = []
    head = "build " + ninja_escape(output)
    if implicit_outputs:
        lines.append(head + " |")
        lines.extend(ninja_escape(path) for path in implicit_outputs)
        head = ": dyndep"
    else:
        head += " : dyndep"
    if implicit_inputs:
        lines.append(head + " |")
        lines.extend(ninja_escape(path) for path in implicit_inputs)
    else:
        lines.append(head)
    text = " $\n    ".join(lines) + "\n"
    if restat:
        text += "  restat = 1\n"
    return text


# Ninja edges as rattler-build steps ---------------------------------------


def run_scanner(ninja: NinjaFile, edge: Edge) -> None:
    """Runs the command of `edge` as Ninja would: after creating the
    directories of its outputs, with `/bin/sh -c`, or directly on Windows."""
    command = edge_variable(ninja, edge, "command", escape=True)
    for output in edge.all_outputs():
        directory = os.path.dirname(output)
        if directory:
            os.makedirs(directory, exist_ok=True)
    print(edge_variable(ninja, edge, "description", escape=True) or command, flush=True)
    status = subprocess.run(command, shell=os.name != "nt", check=False).returncode
    if status != 0:
        raise StepError(
            f"{edge.location}: the scanner failed with status {status}: {command}"
        )


def needed_edges(ninja: NinjaFile) -> list[Edge]:
    """The edges Ninja runs to build the `default` targets, or every target
    no edge reads if there is no `default`, in build file order."""
    targets = ninja.defaults
    if not targets:
        read = {path for edge in ninja.edges for path in edge.all_inputs()}
        targets = [
            path
            for edge in ninja.edges
            for path in edge.all_outputs()
            if path not in read
        ]
    needed: set[Edge] = set()
    pending = [ninja.producers[target] for target in targets]
    while pending:
        edge = pending.pop()
        if edge in needed:
            continue
        needed.add(edge)
        pending.extend(
            ninja.producers[path]
            for path in edge.all_inputs()
            if path in ninja.producers
        )
    return [edge for edge in ninja.edges if edge in needed]


def step_ids(edges: list[Edge]) -> dict[Edge, str]:
    """Names the step of every edge after its rule, numbering the edges of a
    rule used more than once in build file order."""
    uses = Counter(edge.rule.name for edge in edges)
    numbers: Counter[str] = Counter()
    ids: dict[Edge, str] = {}
    for edge in edges:
        name = edge.rule.name
        if uses[name] > 1:
            numbers[name] += 1
            name = f"{name}.{numbers[name]}"
        ids[edge] = name
    if len(set(ids.values())) != len(ids):
        raise NinjaError("step ids derived from the rule names collide; rename a rule")
    return ids


def work_path(path: str, where: str) -> dict[str, str]:
    """Declares the Ninja path `path` relative to the work directory, which is
    the Ninja build directory when rattler-build runs the steps."""
    if path == "." or path == ".." or path.startswith(("/", "../")) or ":" in path:
        raise NinjaError(f"{where}: '{path}' is not inside the build directory")
    return {"root": "work", "path": path}


def edge_step(
    ninja: NinjaFile, edge: Edge, ids: dict[Edge, str], scanner_outputs: set[str]
) -> dict[str, object]:
    """The generated step running `edge`: its explicit, implicit and dyndep
    paths as inputs and outputs, its order-only inputs built by other
    exported edges as `depends_on`, and its command as `run`."""
    command = edge_variable(ninja, edge, "command", escape=True)
    problem = cmd_exe_problem(command) if os.name == "nt" else None
    if problem:
        raise NinjaError(f"{edge.location}: {problem}: {command}")
    depends_on = []
    for path in edge.order_only:
        producer = ninja.producers.get(path)
        # A source file exists before any step runs, and a generated step does
        # not exist before the scanners ran: neither needs an edge.
        if producer is None or path in scanner_outputs:
            continue
        depends_on.append(ids[producer])
    step: dict[str, object] = {
        "id": ids[edge],
        "run": command,
        "inputs": [
            work_path(path, edge.location)
            for path in unique(
                edge.inputs + edge.implicit_inputs + edge.discovered_inputs
            )
        ],
        "outputs": [work_path(path, edge.location) for path in edge.all_outputs()],
    }
    if depends_on:
        step["depends_on"] = unique(depends_on)
    return step


def parse_overrides(assignments: Sequence[str]) -> dict[str, str]:
    overrides = {}
    for assignment in assignments:
        name, separator, value = assignment.partition("=")
        if not separator or not IDENTIFIER.fullmatch(name):
            raise StepError(f"--set {assignment!r}: expected NAME=VALUE")
        overrides[name] = value
    return overrides


# Subcommands --------------------------------------------------------------


def cmd_scan(args: argparse.Namespace) -> None:
    with open_archive(args.archive) as tar:
        members = read_members(tar)
    paths = [extracted_path(args.dest, member.path) for member in members]
    stamp = canonical_path(args.stamp)
    consumers = [canonical_path(consumer) for consumer in args.consumers]
    if len({stamp, *consumers}) != 1 + len(consumers):
        raise StepError("--stamp and --consumers must name different build statements")
    for path in [stamp, *consumers, *paths]:
        if "|" in path or "\n" in path:
            raise StepError(f"{path!r} cannot be written in a Ninja file")
    text = "ninja_dyndep_version = 1\n"
    text += dyndep_statement(stamp, paths, [], restat=True)
    for consumer in consumers:
        text += dyndep_statement(consumer, [], paths, restat=False)
    write_text(args.dyndep, text)


def cmd_extract(args: argparse.Namespace) -> None:
    dest = canonical_path(args.dest)
    with open_archive(args.archive) as tar:
        members = read_members(tar)
        for member in members:
            target = prepare_file(dest, member.path)
            source = tar.extractfile(member.info)
            if source is None:
                raise StepError(f"archive member {member.path!r}: no file data")
            with source, open(target, "xb") as sink:
                shutil.copyfileobj(source, sink)
            os.chmod(target, 0o755 if member.info.mode & 0o111 else 0o644)
    stamp = {
        "version": 1,
        "archive": args.archive,
        "dest": dest,
        "members": [member.path for member in members],
    }
    write_json(args.stamp, stamp)


def cmd_digest(args: argparse.Namespace) -> None:
    dest, members = read_stamp(args.stamp)
    lines = []
    for member in members:
        checksum = hashlib.sha256()
        with open(member_file(dest, member), "rb") as file:
            for chunk in iter(lambda: file.read(1 << 16), b""):
                checksum.update(chunk)
        lines.append(f"{checksum.hexdigest()}  {member}\n")
    write_text(args.output, "".join(lines))


def decode_text(data: bytes) -> str | None:
    """`data` as text, or `None` for binary data: with NUL bytes or not UTF-8."""
    if b"\0" in data:
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return None


def cmd_stats(args: argparse.Namespace) -> None:
    dest, members = read_stamp(args.stamp)
    files = []
    for member in members:
        with open(member_file(dest, member), "rb") as file:
            data = file.read()
        entry: dict[str, object] = {"path": member, "bytes": len(data)}
        text = decode_text(data)
        if text is None:
            entry["text"] = False
        else:
            lines = text.count("\n") + (1 if text and not text.endswith("\n") else 0)
            entry.update(text=True, lines=lines, words=len(text.split()))
        files.append(entry)
    write_json(args.output, {"version": 1, "files": files})


def cmd_summarize(args: argparse.Namespace) -> None:
    checksums: dict[str, str] = {}
    with open(args.checksums, encoding="utf-8") as file:
        for number, line in enumerate(file, 1):
            checksum, separator, path = line.rstrip("\n").partition("  ")
            if not separator or not re.fullmatch(r"[0-9a-f]{64}", checksum):
                raise StepError(f"{args.checksums}:{number}: not a sha256sum line")
            checksums[path] = checksum
    with open(args.stats, encoding="utf-8") as file:
        stats = json.load(file)
    if not isinstance(stats, dict) or stats.get("version") != 1:
        raise StepError(f"{args.stats}: not a version 1 stats file")
    files = sorted(stats["files"], key=lambda entry: entry["path"])
    if [entry["path"] for entry in files] != sorted(checksums):
        raise StepError(f"{args.checksums} and {args.stats} list different files")
    text_files = [entry for entry in files if entry["text"]]
    lines = [
        "Archive summary",
        "===============",
        "",
        f"files: {len(files)} ({len(text_files)} text, {len(files) - len(text_files)} binary)",
        f"bytes: {sum(entry['bytes'] for entry in files)}",
        f"lines: {sum(entry['lines'] for entry in text_files)} in text files",
        f"words: {sum(entry['words'] for entry in text_files)} in text files",
        "",
        f"{'sha256':<16}  {'bytes':>8}  {'lines':>6}  {'words':>6}  path",
    ]
    for entry in files:
        lines_count, words = (
            (entry["lines"], entry["words"]) if entry["text"] else ("-", "-")
        )
        lines.append(
            f"{checksums[entry['path']][:16]}  {entry['bytes']:>8}  "
            f"{lines_count:>6}  {words:>6}  {entry['path']}"
        )
    write_text(args.output, "\n".join(lines) + "\n")


def cmd_export(args: argparse.Namespace) -> None:
    manifest_path = args.manifest or os.environ.get(MANIFEST_ENV)
    if not manifest_path:
        raise StepError(
            f"{MANIFEST_ENV} is not set; pass --manifest outside rattler-build"
        )
    inputs_path = args.inputs or os.environ.get(INPUTS_ENV)
    ninja = parse_ninja(args.ninja_file, parse_overrides(args.overrides))

    dyndep_files = sorted({edge.dyndep for edge in ninja.edges if edge.dyndep})
    if not dyndep_files:
        raise StepError(f"{args.ninja_file}: no build statement has a dyndep binding")
    scanner_set = {
        ninja.producers[path] for path in dyndep_files if path in ninja.producers
    }
    scanners = [edge for edge in ninja.edges if edge in scanner_set]
    for scanner in scanners:
        if scanner.dyndep:
            raise NinjaError(
                f"{scanner.location}: scanners with a dyndep binding are not supported"
            )
        for path in scanner.all_inputs():
            producer = ninja.producers.get(path)
            if producer is not None:
                raise NinjaError(
                    f"{scanner.location}: the scanner reads '{path}', which {producer.location} "
                    "builds; this adapter only runs scanners whose inputs exist before it runs"
                )

    # Ninja's part up to loading the dyndep files: only the scanners run.
    for scanner in scanners:
        run_scanner(ninja, scanner)
    for path in dyndep_files:
        apply_dyndep(ninja, path, parse_dyndep(path))

    # Every other edge becomes a step generated by the step running this.
    exported = [edge for edge in needed_edges(ninja) if edge not in scanner_set]
    ids = step_ids(exported)
    scanner_outputs = {path for scanner in scanners for path in scanner.all_outputs()}
    steps = [edge_step(ninja, edge, ids, scanner_outputs) for edge in exported]
    write_json(manifest_path, {"version": 1, "steps": steps}, atomic=False)
    for edge in exported:
        print(
            f"generated step {ids[edge]}: {len(edge.discovered_outputs)} discovered outputs, "
            f"{len(edge.discovered_inputs)} discovered inputs"
        )

    if inputs_path:
        read = [canonical_path(args.ninja_file)]
        for scanner in scanners:
            read += scanner.inputs + scanner.implicit_inputs
        report = [work_path(path, args.ninja_file) for path in unique(read)]
        write_json(inputs_path, {"version": 1, "inputs": report}, atomic=False)


def prefix_path(subdir: str) -> str:
    """The directory `subdir` of the host prefix of the rattler-build step."""
    prefix = os.environ.get("PREFIX")
    if not prefix:
        raise StepError(
            "PREFIX is not set; install-members and install-files run in build steps"
        )
    components = [
        component for component in subdir.split("/") if component not in ("", ".")
    ]
    if subdir.startswith("/") or not components or ".." in components:
        raise StepError(f"{subdir!r}: expected a directory inside the prefix")
    return os.path.join(prefix, *components)


def cmd_install_members(args: argparse.Namespace) -> None:
    dest, members = read_stamp(args.stamp)
    root = prefix_path(args.subdir)
    os.makedirs(root, exist_ok=True)
    for member in members:
        target = os.path.join(root, *member.split("/"))
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copy(member_file(dest, member), target)
    print(f"installed {len(members)} files into {root}")


def cmd_install_files(args: argparse.Namespace) -> None:
    root = prefix_path(args.subdir)
    os.makedirs(root, exist_ok=True)
    for path in args.files:
        shutil.copy(path, os.path.join(root, os.path.basename(path)))


def sample_files() -> list[tuple[str, bytes]]:
    """The files of the sample archive, generated rather than checked in."""
    primes = [n for n in range(2, 100) if all(n % d for d in range(2, int(n**0.5) + 1))]
    fibonacci = [0, 1]
    while len(fibonacci) < 30:
        fibonacci.append(fibonacci[-1] + fibonacci[-2])
    readme = (
        "Sample archive of the ninja-archive example of rattler-build.\n"
        "\n"
        "Neither build.ninja nor recipe.yaml lists the files in this archive.\n"
        "The scanner reads them from the archive and declares them as outputs\n"
        "of the extraction and as inputs of the steps reading them, before any\n"
        "of these steps runs.\n"
    )
    note = (
        "Ninja dynamic dependencies (dyndep, Ninja 1.10 and later)\n"
        "\n"
        "A build statement with `dyndep = file` has `file` as an input. Once\n"
        "`file` is built, Ninja loads the implicit inputs and outputs it lists\n"
        "for the statement, before the statement runs.\n"
        "\n"
        "The name of this file contains a space, which Ninja spells `$ `.\n"
    )
    primes_csv = "index,prime\n" + "".join(
        f"{i},{p}\n" for i, p in enumerate(primes, 1)
    )
    return [
        ("sample/README.txt", readme.encode("utf-8")),
        ("sample/data/bytes.bin", bytes(range(256))),
        (
            "sample/data/fibonacci.txt",
            "".join(f"{n}\n" for n in fibonacci).encode("utf-8"),
        ),
        ("sample/data/primes.csv", primes_csv.encode("utf-8")),
        ("sample/docs/ninja dyndep.txt", note.encode("utf-8")),
    ]


def cmd_sample(args: argparse.Namespace) -> None:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for directory in ("sample", "sample/data", "sample/docs"):
            info = tarfile.TarInfo(directory)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.mtime = SAMPLE_MTIME
            tar.addfile(info)
        for name, data in sample_files():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o644
            info.mtime = SAMPLE_MTIME
            tar.addfile(info, io.BytesIO(data))
    write_bytes(args.output, buffer.getvalue())


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="archive_steps.py",
        description="Tar archive extraction as a Ninja dyndep graph, and its rattler-build adapter.",
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    command = commands.add_parser(
        "sample", help="write the deterministic sample archive"
    )
    command.add_argument("output", help="tar archive to write")
    command.set_defaults(run=cmd_sample)

    command = commands.add_parser(
        "scan", help="write the Ninja dyndep file of an archive"
    )
    command.add_argument("archive")
    command.add_argument("dyndep", help="dyndep file to write")
    command.add_argument(
        "--stamp",
        required=True,
        help="output of the extraction edge, which gains every member as an implicit output",
    )
    command.add_argument(
        "--dest", required=True, help="directory the extraction edge unpacks into"
    )
    command.add_argument(
        "--consumers",
        nargs="*",
        default=[],
        metavar="OUTPUT",
        help="outputs of the edges that gain every member as an implicit input",
    )
    command.set_defaults(run=cmd_scan)

    command = commands.add_parser(
        "extract", help="unpack an archive and write its stamp"
    )
    command.add_argument("archive")
    command.add_argument("stamp", help="stamp to write, listing the unpacked members")
    command.add_argument("--dest", required=True, help="directory to unpack into")
    command.set_defaults(run=cmd_extract)

    command = commands.add_parser(
        "digest", help="write sha256sum lines of the unpacked members"
    )
    command.add_argument("stamp")
    command.add_argument("output")
    command.set_defaults(run=cmd_digest)

    command = commands.add_parser(
        "stats", help="write byte, line and word counts as JSON"
    )
    command.add_argument("stamp")
    command.add_argument("output")
    command.set_defaults(run=cmd_stats)

    command = commands.add_parser(
        "summarize", help="write a report of checksums and counts"
    )
    command.add_argument("checksums")
    command.add_argument("stats")
    command.add_argument("output")
    command.set_defaults(run=cmd_summarize)

    command = commands.add_parser(
        "export",
        help="run the Ninja scanners and declare the other edges as build steps",
    )
    command.add_argument("ninja_file")
    command.add_argument(
        "--set",
        dest="overrides",
        action="append",
        default=[],
        metavar="NAME=VALUE",
        help="bind a top-level Ninja variable to VALUE instead",
    )
    command.add_argument(
        "--manifest", help=f"step manifest to write (default: ${MANIFEST_ENV})"
    )
    command.add_argument(
        "--inputs", help=f"input report to write (default: ${INPUTS_ENV})"
    )
    command.set_defaults(run=cmd_export)

    command = commands.add_parser(
        "install-members", help="copy the unpacked members into $PREFIX"
    )
    command.add_argument("stamp")
    command.add_argument("subdir", help="directory in $PREFIX to copy into")
    command.set_defaults(run=cmd_install_members)

    command = commands.add_parser("install-files", help="copy files into $PREFIX")
    command.add_argument("subdir", help="directory in $PREFIX to copy into")
    command.add_argument("files", nargs="+")
    command.set_defaults(run=cmd_install_files)

    args = parser.parse_args(argv)
    try:
        args.run(args)
    except (StepError, OSError, tarfile.TarError, json.JSONDecodeError) as error:
        print(f"archive_steps.py {args.command}: error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
