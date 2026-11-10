#!/usr/bin/env python3
"""Manual, development-only load/capture checks. Agents must NEVER execute this script.

Run from another workspace: python3 scripts/integration.py --workspace 9
Only the user may run it, in a disposable Sway session. No cleanup follows a failure.
"""

import argparse
import json
import math
import os
from pathlib import Path
import re
import secrets
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile
import time


IPC_TIMEOUT = 4
WINDOW_TIMEOUT_MS = 20000
TERMINAL_LIFETIME = 300
HARMLESS_EVENTS = {"title", "urgent", "mark"}


class Failure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def run(arguments, operation, timeout=IPC_TIMEOUT):
    try:
        result = subprocess.run(
            arguments, stdin=subprocess.DEVNULL, capture_output=True,
            text=True, encoding="utf-8", timeout=timeout, check=False,
        )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as error:
        raise Failure(f"{operation}: {error}") from error
    require(result.returncode == 0,
            f"{operation}: exit {result.returncode}: {result.stderr.strip()}")
    return result.stdout


def ipc(sway, message_type, payload=None):
    arguments = sway + ["-t", message_type]
    if payload is not None:
        arguments.append(payload)
    output = run(arguments, f"Sway {message_type}")
    try:
        return json.loads(output)
    except ValueError as error:
        raise Failure(f"Sway {message_type}: invalid JSON") from error


def command(sway, text):
    replies = ipc(sway, "command", text)
    require(isinstance(replies, list) and len(replies) == 1 and isinstance(replies[0], dict)
            and replies[0].get("success") is True,
            f"Sway command {text!r}: {replies!r}")


class Events:
    """One swaymsg subscription, retained across commands to catch away-and-back focus.

    Popen is needed only for this streaming command; every read and shutdown is bounded.
    Raw monitor output is one flushed JSON object per line (swaymsg/main.c).
    """

    def __init__(self, sway):
        self.sway = sway
        self.buffer = b""
        self.focus_safe = True
        self.current = None
        self.reader = selectors.DefaultSelector()
        self.process = subprocess.Popen(
            sway + ["-m", "-t", "subscribe", '["workspace","window","tick"]'],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        self.reader.register(self.process.stdout, selectors.EVENT_READ)

    def require_control(self, condition, message):
        # Unlike an ordinary check failure, uncertain user control forbids restoration.
        if not condition:
            self.focus_safe = False
            raise Failure(message)

    def read(self, deadline):
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and self.reader.select(remaining),
                    "event observation deadline expired")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            require(chunk, "Sway event subscription disconnected")
            self.buffer += chunk
            require(len(self.buffer) <= 1024 * 1024, "event buffer exceeded 1 MiB")
        line, self.buffer = self.buffer.split(b"\n", 1)
        event = json.loads(line)
        require(isinstance(event, dict), "invalid event object")
        return event

    def fence(self, transition=None):
        # The first tick proves subscription readiness; later unique ticks prove that
        # prior events reached us. A snapshot and a command still cannot be atomic.
        deadline = time.monotonic() + IPC_TIMEOUT
        token = secrets.token_hex(16)
        windows = []
        switched = False
        try:
            require(ipc(self.sway, "send_tick", token).get("success") is True,
                    "Sway rejected event fence")
            for _ in range(4096):
                event = self.read(deadline)
                if "payload" in event:
                    if event.get("payload") == token and event.get("first") is False:
                        require(transition is None or switched,
                                "workspace switch produced no expected focus event")
                        return windows
                    continue
                if "container" in event:
                    windows.append(event)
                    continue

                change = event.get("change")
                current = event.get("current") or {}
                if change == "focus":
                    if transition is not None and not switched:
                        old_id, destination = transition
                        require((event.get("old") or {}).get("id") == old_id
                                and current.get("name") == destination,
                                f"unexpected workspace switch: {event!r}")
                        self.current = current
                        switched = True
                    elif self.current is not None:
                        require(current.get("id") == self.current["id"]
                                and (event.get("old") or {}).get("id")
                                in (None, self.current["id"]),
                                "workspace focus changed; user control takes precedence")
                    else:
                        raise Failure("workspace focus changed during initial preflight")
                elif change in {"reload", "rename", "move"}:
                    raise Failure(f"workspace {change} during harness execution")
                elif change == "empty" and self.current is not None:
                    require(current.get("id") != self.current["id"],
                            "focused workspace was destroyed")
                elif change not in {"init", "empty", "urgent"}:
                    raise Failure(f"unexpected workspace event: {event!r}")
            raise Failure("event history exceeded 4096 events")
        except (Failure, OSError, ValueError, KeyError, TypeError):
            self.focus_safe = False
            raise

    def close(self):
        self.reader.close()
        # This is our IPC helper only, never a terminal or application process.
        self.process.terminate()
        try:
            self.process.wait(timeout=IPC_TIMEOUT)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=IPC_TIMEOUT)
        self.process.stdout.close()


def walk(container, workspace=None, unsafe=False):
    if container["type"] == "workspace":
        workspace = container
        unsafe = unsafe or container.get("name") == "__i3_scratch"
    else:
        unsafe = unsafe or container.get("fullscreen_mode") not in (None, 0)
    unsafe = (unsafe or container["type"] == "floating_con"
              or container.get("floating") in {"auto_on", "user_on"}
              or container.get("scratchpad_state") in {"fresh", "changed"})
    yield container, workspace, unsafe
    for child in container["nodes"]:
        yield from walk(child, workspace, unsafe)
    for child in container["floating_nodes"]:
        yield from walk(child, workspace, True)


def snapshot(sway, events, transition=None):
    workspaces = ipc(sway, "get_workspaces")
    tree = ipc(sway, "get_tree")
    history = events.fence(transition)
    containers = list(walk(tree))
    ids = [container["id"] for container, _, _ in containers]
    events.require_control(tree["type"] == "root" and len(ids) == len(set(ids)),
            "invalid tree or duplicate container identities")
    workspace_ids = [workspace["id"] for workspace in workspaces]
    workspace_names = [workspace["name"] for workspace in workspaces]
    events.require_control(len(workspace_ids) == len(set(workspace_ids))
            and len(workspace_names) == len(set(workspace_names)),
            "ambiguous workspace identities or names")
    focused = [workspace for workspace in workspaces if workspace["focused"]]
    events.require_control(len(focused) == 1, "expected exactly one focused workspace")
    focused = focused[0]
    if events.current is not None:
        if (focused["id"], focused["name"]) != (
                events.current["id"], events.current["name"]):
            events.focus_safe = False
            raise Failure("workspace focus or identity changed")
    focused_nodes = [(container, workspace) for container, workspace, _ in containers
                     if container.get("focused")]
    events.require_control(len(focused_nodes) == 1 and focused_nodes[0][1] is not None
            and focused_nodes[0][1]["id"] == focused["id"],
            "tree and workspace focus disagree")
    tree_workspaces = {container["id"]: container for container, _, _ in containers
                       if container["type"] == "workspace"
                       and container.get("name") != "__i3_scratch"}
    events.require_control(set(tree_workspaces) == set(workspace_ids),
            "workspace query and raw tree identities disagree")
    for workspace in workspaces:
        events.require_control(tree_workspaces[workspace["id"]]["name"] == workspace["name"],
                "workspace query and raw tree names disagree")
    return tree, workspaces, tree_workspaces[focused["id"]], history


def require_quiet(history, events, closing_id=None, remaining_ids=()):
    for event in history:
        change = event.get("change")
        container_id = event["container"]["id"]
        if change in HARMLESS_EVENTS:
            continue
        if closing_id is not None:
            if change == "close" and container_id == closing_id:
                continue
            if change == "focus" and container_id in remaining_ids:
                continue
        events.focus_safe = False
        raise Failure(f"unexpected window activity: {change} id={container_id}")


def empty(workspace):
    require(not workspace["nodes"] and not workspace["floating_nodes"],
            f"workspace {workspace['name']!r} is occupied: {outline(workspace)}")


def window(number, weight=1):
    return {"id": f"window{number}", "weight": weight}


def split(axis, children, weight=1):
    return {"split": axis, "children": children, "weight": weight}


def leaves(layout):
    if "id" in layout:
        yield layout
    else:
        for child in layout["children"]:
            yield from leaves(child)


def scenarios(run_id, foot_binary, sleep_binary):
    layouts = [
        ("single-window", window(1), "window1"),
        ("horizontal-pair", split("horizontal", [window(1, 3), window(2, 2)]), "window2"),
        ("vertical-pair", split("vertical", [window(1, 2), window(2, 3)]), "window1"),
        ("uneven-siblings", split("horizontal", [window(1, 5), window(2, 3), window(3, 2)]),
         "window2"),
        ("mixed-nesting", split("horizontal", [
            window(1, 4), split("vertical", [window(2, 3), window(3, 2)], 3),
            window(4, 3),
        ]), "window3"),
        ("deep-nesting", split("vertical", [
            split("horizontal", [
                window(1, 2), split("horizontal", [
                    window(2, 2), split("vertical", [window(3, 3), window(4, 2)], 3),
                ], 3),
            ], 3), window(5, 2),
        ]), "window4"),
    ]
    for name, layout, focus in layouts:
        for leaf in leaves(layout):
            leaf["app_id"] = f"sway-stage-it-{run_id}-{name}-{leaf['id']}"
            # Foot's documented syntax is foot [OPTIONS] command [ARGUMENTS].
            # The bounded child avoids leaving an interactive shell after an aborted run.
            leaf["command"] = [foot_binary, f"--app-id={leaf['app_id']}", sleep_binary,
                               str(TERMINAL_LIFETIME)]
        yield name, layout, focus


def yaml_document(layout, focus, shares=None):
    """Emit our fixed schema, also serving as the capture format's expected skeleton."""
    draft = shares is not None
    lines = ["version: 1", f"focus: {focus}"]
    if not draft:
        lines.append(f"window_timeout_ms: {WINDOW_TIMEOUT_MS}")
    lines.append("layout:")

    def emit(configured, indent, path, sequence=False):
        fields = []
        if "id" in configured:
            fields.append(f"id: {configured['id']}")
        else:
            fields.append(f"split: {configured['split']}")
        if path:
            weight = shares[path] if draft else configured["weight"]
            fields.append(f"weight: {weight}")
        if "id" in configured:
            command_value = "null" if draft else json.dumps(configured["command"])
            fields += [f"command: {command_value}", "match:",
                       f"  app_id: {configured['app_id']}"]
        else:
            fields.append("children:")

        for field_index, field in enumerate(fields):
            prefix = " " * indent
            if sequence:
                prefix += "- " if field_index == 0 else "  "
            lines.append(prefix + field)
        if "children" in configured:
            child_indent = indent + (4 if sequence else 2)
            for child_index, child in enumerate(configured["children"]):
                emit(child, child_indent, path + (child_index,), True)

    emit(layout, 2, ())
    return "\n".join(lines) + "\n"


def is_window(container):
    return any(container.get(key) is not None for key in ("app_id", "shell", "window"))


def outline(container):
    children = container.get("nodes", []) + container.get("floating_nodes", [])
    label = f"{container.get('id')}:{container.get('app_id') or container.get('layout')}"
    if children:
        label += "(" + ", ".join(outline(child) for child in children) + ")"
    return label


def shape(container, events):
    """Identity-bearing hierarchy; only validated single-child wrappers disappear."""
    events.require_control(container["type"] in {"workspace", "con"},
            f"unexpected container type: {outline(container)}")
    events.require_control(not container["floating_nodes"]
            and container.get("floating") not in {"auto_on", "user_on"}
            and container.get("scratchpad_state") in (None, "none")
            and (container["type"] == "workspace" or container.get("fullscreen_mode") == 0),
            f"floating/fullscreen/scratchpad state: {outline(container)}")
    if is_window(container):
        events.require_control(container["type"] == "con" and not container["nodes"],
                f"malformed window: {outline(container)}")
        return (container["id"], container.get("app_id"))
    events.require_control(container["layout"] in {"splith", "splitv"},
            f"unsupported group: {outline(container)}")
    children = tuple(shape(child, events) for child in container["nodes"])
    if len(children) == 1:
        return children[0]
    return (container["id"], container["layout"], children)


def signature(workspace):
    # Ignore titles/urgency/marks and focus history, but include all geometry, identities,
    # state flags, actual wrappers, and current focus. Capture must not change any of it.
    fields = ("id", "type", "layout", "app_id", "window", "shell", "rect", "focused",
              "floating", "fullscreen_mode", "scratchpad_state")
    return ({key: workspace.get(key) for key in fields},
            [signature(child) for child in workspace["nodes"]],
            [signature(child) for child in workspace["floating_nodes"]])


def ownership(tree, target, layout, preexisting_ids, events, accepted=None):
    expected_apps = {leaf["app_id"] for leaf in leaves(layout)}
    found = {}
    for container, workspace, unsafe in walk(tree):
        app_id = container.get("app_id")
        if app_id not in expected_apps:
            continue
        container_id = container["id"]
        events.require_control(container_id not in preexisting_ids and workspace is not None
                and workspace["id"] == target and not unsafe and is_window(container),
                f"ownership/placement uncertain: id={container_id} app_id={app_id} "
                f"workspace={None if workspace is None else workspace['name']!r}")
        events.require_control(app_id not in found.values(), f"ambiguous app ID: {app_id}")
        found[container_id] = app_id
    if accepted is None:
        events.require_control(set(found.values()) == expected_apps,
                f"window identities: expected {sorted(expected_apps)!r}, observed {found!r}")
    else:
        events.require_control(found == accepted,
                f"owned windows changed: expected {accepted!r}, observed {found!r}")
    return found


def verify_layout(workspace, layout, focus, accepted, events):
    shape(workspace, events)  # Validate even wrappers omitted by the comparison below.
    observed_leaves = [container for container, _, _ in walk(workspace) if is_window(container)]
    events.require_control({container["id"] for container in observed_leaves} == set(accepted),
            f"unexpected windows: {outline(workspace)}")
    events.require_control([container["app_id"] for container in observed_leaves]
            == [leaf["app_id"] for leaf in leaves(layout)],
            f"window order differs: {outline(workspace)}")
    focused = [container for container, _, _ in walk(workspace) if container.get("focused")]
    focus_app = next(leaf["app_id"] for leaf in leaves(layout) if leaf["id"] == focus)
    events.require_control(len(focused) == 1 and focused[0].get("app_id") == focus_app,
            f"final focus differs: expected {focus_app}, observed {outline(workspace)}")
    shares = {}
    diagnostics = []

    def compare(configured, observed, path):
        while not is_window(observed) and len(observed["nodes"]) == 1:
            observed = observed["nodes"][0]
        if "id" in configured:
            events.require_control(is_window(observed) and observed["app_id"] == configured["app_id"],
                    f"{path}: expected {configured['id']}, observed {outline(observed)}")
            return
        axis = "splith" if configured["split"] == "horizontal" else "splitv"
        expected_children = configured["children"]
        events.require_control(not is_window(observed) and observed["layout"] == axis
                and len(observed["nodes"]) == len(expected_children),
                f"{path}: expected {axis} with {len(expected_children)} children, "
                f"observed {outline(observed)}")
        dimension = "width" if axis == "splith" else "height"
        coordinate = "x" if axis == "splith" else "y"
        rectangles = [child["rect"] for child in observed["nodes"]]
        extents = [rectangle[dimension] for rectangle in rectangles]
        require(all(extent > 0 for extent in extents), f"{path}: unusable rectangles {rectangles}")
        for previous, following in zip(rectangles, rectangles[1:]):
            events.require_control(previous[coordinate] + previous[dimension] <= following[coordinate],
                    f"{path}: visual order/overlap differs: {rectangles}")
        total_weight = sum(child["weight"] for child in expected_children)
        requested = [100 * child["weight"] / total_weight for child in expected_children]
        measured = [100 * extent / sum(extents) for extent in extents]
        deviations = [abs(actual - expected) for actual, expected in zip(measured, requested)]
        requested_text = ", ".join(f"{share:.2f}" for share in requested)
        measured_text = ", ".join(f"{share:.2f}" for share in measured)
        detail = (f"{path or 'root'} {axis}: expected [{requested_text}], "
                  f"observed [{measured_text}] percent; "
                  f"container={observed['id']}")
        require(max(deviations) <= 5 + 1e-9, f"sizing exceeds 5 percentage points: {detail}")
        if max(deviations) > 1 + 1e-9:
            diagnostics.append(detail)
        for child_index, (expected_child, observed_child) in enumerate(
                zip(expected_children, observed["nodes"])):
            child_path = path + (child_index,)
            shares[child_path] = measured[child_index]
            compare(expected_child, observed_child, child_path)

    try:
        compare(layout, workspace, ())
    except Failure as error:
        expected = yaml_document(layout, focus)
        raise Failure(f"{error}\nExpected layout:\n{expected}Observed: {outline(workspace)}") from error
    return shares, diagnostics


def verify_capture(draft, layout, focus, shares, directory, binary):
    # This is a comparison with one known serializer format, not a YAML parser. A format
    # change fails visibly; check supplies authoritative syntax and semantic validation.
    actual = [line.rstrip() for line in draft.splitlines()
              if line.strip() and not line.lstrip().startswith("#")]
    expected = yaml_document(layout, focus, shares).splitlines()
    require(len(actual) == len(expected),
            f"capture draft has unexpected fields/structure:\n{draft}")
    placeholders = 0
    filled = []
    for actual_line, expected_line in zip(actual, expected):
        actual_field = actual_line.lstrip(" -")
        expected_field = expected_line.lstrip(" -")
        require(actual_line[:len(actual_line) - len(actual_field)]
                == expected_line[:len(expected_line) - len(expected_field)],
                f"capture indentation/arrangement: {actual_line!r}, expected {expected_line!r}")
        key, separator, scalar = actual_field.partition(":")
        expected_key, _, expected_scalar = expected_field.partition(":")
        require(separator and key == expected_key,
                f"capture field: {actual_line!r}, expected {expected_line!r}")
        scalar = scalar.strip()
        expected_scalar = expected_scalar.strip()
        if key == "weight":
            try:
                weight = float(scalar)
            except ValueError as error:
                raise Failure(f"capture weight is not numeric: {actual_line}") from error
            require(math.isfinite(weight) and abs(weight - float(expected_scalar)) <= 0.02,
                    f"capture sizing: {actual_line!r}, expected {expected_line!r}")
        else:
            if key in {"id", "focus", "app_id", "split"}:
                # Our generated identifiers contain no YAML punctuation or escapes.
                scalar = scalar.removeprefix('"').removesuffix('"')
                scalar = scalar.removeprefix("'").removesuffix("'")
            require(scalar == expected_scalar,
                    f"capture value: {actual_line!r}, expected {expected_line!r}")
        if key == "command":
            placeholders += 1
            actual_line = actual_line.replace("command: null", 'command: ["true"]')
        filled.append(actual_line)
    require(placeholders == len(list(leaves(layout))), "capture command placeholder count differs")
    (directory / "captured.yaml").write_text("\n".join(filled) + "\n", encoding="utf-8")
    run([binary, "--layouts-dir", str(directory), "check", "captured"], "check filled capture")


def remove_from_shape(hierarchy, closing_id):
    if len(hierarchy) == 2:
        return None if hierarchy[0] == closing_id else hierarchy
    children = tuple(child for original in hierarchy[2]
                     if (child := remove_from_shape(original, closing_id)) is not None)
    if not children:
        return None
    return children[0] if len(children) == 1 else (hierarchy[0], hierarchy[1], children)


def cleanup(sway, events, layout, preexisting_ids, accepted, baseline):
    remaining = dict(accepted)
    target = events.current["id"]
    for closing_id in accepted:
        tree, _, workspace, history = snapshot(sway, events)
        require_quiet(history, events)
        ownership(tree, target, layout, preexisting_ids, events, remaining)
        events.require_control(signature(workspace) == signature(baseline),
                "workspace changed before cleanup; ownership/state uncertain")
        expected_after = remove_from_shape(shape(workspace, events), closing_id)
        expected_remaining = {container_id: app_id for container_id, app_id in remaining.items()
                              if container_id != closing_id}

        # The only destructive command in the harness. All evidence above is refreshed
        # for every close; there is still an unavoidable query/command race in Sway IPC.
        command(sway, f"[con_id={closing_id}] kill")
        deadline = time.monotonic() + IPC_TIMEOUT
        while True:
            tree, _, workspace, history = snapshot(sway, events)
            require_quiet(history, events, closing_id, expected_remaining)
            current_ids = {container["id"] for container, _, _ in walk(tree)}
            disappeared = closing_id not in current_ids
            expected_owned = expected_remaining if disappeared else remaining
            ownership(tree, target, layout, preexisting_ids, events, expected_owned)
            if disappeared:
                break
            require(time.monotonic() < deadline,
                    f"cleanup timeout: container {closing_id} did not disappear")
            events.require_control(signature(workspace) == signature(baseline),
                    "workspace changed while waiting for close")
            time.sleep(0.05)
        remaining = expected_remaining
        observed_after = shape(workspace, events)
        if remaining:
            events.require_control(observed_after == expected_after,
                    f"cleanup hierarchy changed: expected {expected_after}, observed {observed_after}")
        else:
            empty(workspace)
        baseline = workspace


def preflight(sway, events, workspace_number):
    tree, workspaces, original, history = snapshot(sway, events)
    require_quiet(history, events)
    require(original["name"] != workspace_number
            and all(workspace["num"] != int(workspace_number)
                    for workspace in workspaces if workspace["id"] == original["id"]),
            "disposable workspace is the starting workspace")
    # Restrict the original name before any mutation, so restoration never interpolates
    # command syntax, quoting escapes, or control characters from an unusual name.
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 _.:+\-]*", original["name"]),
            "starting workspace name cannot be safely represented by this harness")
    candidates = [workspace for workspace in workspaces
                  if workspace["name"] == workspace_number
                  or workspace["num"] == int(workspace_number)]
    require(len(candidates) <= 1, "ambiguous disposable workspace number")
    if candidates:
        require(candidates[0]["name"] == workspace_number,
                "workspace number belongs to a differently named workspace; refusing")
        designated = next(container for container, _, _ in walk(tree)
                          if container["id"] == candidates[0]["id"])
        empty(designated)
    events.current = original
    return original, candidates[0]["id"] if candidates else None


def restore(sway, events, original, disposable_name):
    if not events.focus_safe or events.current["name"] != disposable_name:
        return "Original workspace not restored: focus history is uncertain or user took control."
    _, workspaces, _, history = snapshot(sway, events)
    require_quiet(history, events)
    matches = [workspace for workspace in workspaces
               if (workspace["id"], workspace["name"]) == (original["id"], original["name"])]
    if len(matches) != 1:
        return "Original workspace not restored: its recorded identity no longer exists."
    command(sway, f'workspace --no-auto-back-and-forth "{original["name"]}"')
    snapshot(sway, events, (events.current["id"], original["name"]))
    return "Original workspace restored."


def report_remaining(sway, run_id, known):
    print("No further cleanup attempted. Test terminals expire after 300 seconds.", file=sys.stderr)
    try:
        tree = ipc(sway, "get_tree")
        for container, workspace, unsafe in walk(tree):
            app_id = container.get("app_id") or ""
            if app_id.startswith(f"sway-stage-it-{run_id}-"):
                print(f"  observed id={container['id']} app_id={app_id} "
                      f"workspace={None if workspace is None else workspace['name']!r} "
                      f"unsafe_state={unsafe}", file=sys.stderr)
    except (Failure, OSError, ValueError, KeyError, TypeError) as error:
        print(f"  Remaining windows could not be queried: {error}; last accepted={known!r}",
              file=sys.stderr)


def workspace_argument(value):
    if not re.fullmatch(r"[1-9][0-9]{0,8}", value):
        raise argparse.ArgumentTypeError("use an explicit positive workspace number (1–999999999)")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", required=True, type=workspace_argument)
    parser.add_argument("--binary", default=str(
        Path(__file__).resolve().parents[1] / "target/release/sway-stage"))
    arguments = parser.parse_args()
    events = None
    original = None
    switch_attempted = False
    passed = 0
    failed = 0
    stage = "preflight"
    known = {}
    run_id = secrets.token_hex(12)
    sway = None
    restoration = None
    try:
        socket_path = os.environ.get("SWAYSOCK", "")
        require(socket_path and stat.S_ISSOCK(os.stat(socket_path).st_mode),
                "SWAYSOCK must identify an existing Unix socket")
        executables = {}
        for name in ("swaymsg", "foot", "sleep", arguments.binary):
            resolved = shutil.which(name)
            require(resolved is not None, f"required executable not found: {name}")
            executables[name] = str(Path(resolved).resolve())
        binary = executables[arguments.binary]
        sway = [executables["swaymsg"], "-s", socket_path, "-r"]
        # Multi-seat focus semantics are outside this harness's safety assumptions.
        seats = ipc(sway, "get_seats")
        require(len(seats) == 1, "this harness requires exactly one Sway seat")
        events = Events(sway)
        # Wait for the initial subscription tick before establishing our starting state.
        first = events.read(time.monotonic() + IPC_TIMEOUT)
        require(first.get("first") is True, "missing initial subscription tick")
        original, _ = preflight(sway, events, arguments.workspace)

        with tempfile.TemporaryDirectory(prefix="sway-stage-integration-") as temporary:
            directory = Path(temporary)
            prepared = list(scenarios(run_id, executables["foot"], executables["sleep"]))
            for name, layout, focus in prepared:
                (directory / f"{name}.yaml").write_text(
                    yaml_document(layout, focus), encoding="utf-8")
                run([binary, "--layouts-dir", temporary, "check", name], f"check {name}")

            # Recheck all preconditions after preparation, immediately before switching.
            original, designated_id = preflight(sway, events, arguments.workspace)
            stage = "workspace switch"
            switch_attempted = True
            command(sway, f"workspace --no-auto-back-and-forth {arguments.workspace}")
            _, _, workspace, history = snapshot(
                sway, events, (original["id"], arguments.workspace))
            require_quiet(history, events)
            events.require_control(designated_id is None or workspace["id"] == designated_id,
                    "disposable workspace identity changed during the switch")
            empty(workspace)

            for name, layout, focus in prepared:
                stage = f"{name}: empty-workspace precondition"
                known = {}
                tree, _, workspace, history = snapshot(sway, events)
                require_quiet(history, events)
                empty(workspace)
                preexisting_ids = {container["id"] for container, _, _ in walk(tree)}

                stage = f"{name}: load"
                timeout = len(list(leaves(layout))) * WINDOW_TIMEOUT_MS / 1000 + 30
                load_output = run([binary, "--layouts-dir", temporary, "load", name], stage, timeout)
                require(not load_output.strip(), "load produced unexpected stdout")

                stage = f"{name}: raw-tree verification"
                tree, _, workspace, history = snapshot(sway, events)
                expected_apps = {leaf["app_id"] for leaf in leaves(layout)}
                observed_windows = {}
                for event in history:
                    change = event.get("change")
                    container = event["container"]
                    if change in HARMLESS_EVENTS:
                        continue
                    app_id = container.get("app_id")
                    events.require_control(change in {"new", "focus"}
                            and container["id"] not in preexisting_ids
                            and (app_id is None or app_id in expected_apps),
                            f"unexpected activity during load: {change} id={container['id']}")
                    previous_app = observed_windows.get(container["id"])
                    events.require_control(previous_app is None or app_id in (None, previous_app),
                            f"event identity changed during load: id={container['id']} "
                            f"app_id={previous_app!r} -> {app_id!r}")
                    # Keep null identities provisional, and retain any known identity so
                    # a later event or tree cannot silently substitute another window.
                    if container["id"] not in observed_windows or app_id is not None:
                        observed_windows[container["id"]] = app_id
                known = ownership(tree, workspace["id"], layout, preexisting_ids, events)
                for container_id, observed_app in observed_windows.items():
                    events.require_control(container_id in known
                            and (observed_app is None or observed_app == known[container_id]),
                            f"event window did not resolve to an accepted window: "
                            f"id={container_id} event_app_id={observed_app!r}")
                shares, sizing_notes = verify_layout(workspace, layout, focus, known, events)
                before_capture = signature(workspace)

                stage = f"{name}: capture"
                draft = run([binary, "capture"], stage, 15)
                tree, _, workspace, history = snapshot(sway, events)
                require_quiet(history, events)
                ownership(tree, workspace["id"], layout, preexisting_ids, events, known)
                events.require_control(signature(workspace) == before_capture,
                                       "capture changed the workspace")
                verify_capture(draft, layout, focus, shares, directory, binary)

                stage = f"{name}: cleanup"
                cleanup(sway, events, layout, preexisting_ids, known, workspace)
                _, _, workspace, history = snapshot(sway, events)
                require_quiet(history, events)
                empty(workspace)
                passed += 1
                print(f"PASS  {name:19} hierarchy, sizing, focus, identity, capture", flush=True)
                for note in sizing_notes:
                    print(f"      above 1-point sizing target: {note}", flush=True)
            stage = "final restoration"
            restoration = restore(sway, events, original, arguments.workspace)
            require(restoration == "Original workspace restored.", restoration)
    except (Failure, OSError, ValueError, KeyError, TypeError, StopIteration, KeyboardInterrupt) as error:
        failed = 1
        if isinstance(error, KeyboardInterrupt) and events is not None:
            events.focus_safe = False
        print(f"FAIL  {stage}: {error or 'interrupted'}", file=sys.stderr, flush=True)
        if switch_attempted and sway is not None:
            report_remaining(sway, run_id, known)
        if switch_attempted and original is not None and stage != "final restoration":
            try:
                restoration = restore(sway, events, original, arguments.workspace)
            except (Failure, OSError, ValueError, KeyError, TypeError) as restore_error:
                restoration = f"Original workspace not restored: {restore_error}"
    finally:
        if events is not None:
            try:
                events.close()
            except (OSError, subprocess.TimeoutExpired) as error:
                failed = 1
                print(f"FAIL  event helper shutdown: {error}", file=sys.stderr)
    print(f"\n{passed} passed, {failed} failed")
    if restoration:
        print(restoration)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
