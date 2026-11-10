#!/usr/bin/env python3
"""Manual, development-only load/capture checks. Agents must NEVER execute this script.

Run from another workspace: python3 scripts/integration.py --workspace 9
Add --extended for optional Firefox/Xterm scenarios and repeated Foot app IDs.
Only the user may run it, in a disposable Sway session. No cleanup follows a failure.
"""

import argparse
import configparser
from dataclasses import dataclass
import itertools
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
FIREFOX_TIMEOUT_MS = 60000
TERMINAL_LIFETIME = 300
HARMLESS_EVENTS = {"title", "urgent", "mark"}


class Failure(Exception):
    pass


@dataclass
class Scenario:
    name: str
    layout: dict
    focus: str
    window_timeout_ms: int = WINDOW_TIMEOUT_MS
    skip: str = ""


def require(condition, message):
    if not condition:
        raise Failure(message)


def run(arguments, operation, timeout=IPC_TIMEOUT, env=None):
    try:
        result = subprocess.run(
            arguments, stdin=subprocess.DEVNULL, capture_output=True,
            text=True, encoding="utf-8", timeout=timeout, check=False, env=env,
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
            app_id = f"sway-stage-it-{run_id}-{name}-{leaf['id']}"
            leaf["match"] = {"app_id": app_id}
            # Foot's documented syntax is foot [OPTIONS] command [ARGUMENTS].
            # The bounded child avoids leaving an interactive shell after an aborted run.
            leaf["command"] = [foot_binary, f"--app-id={app_id}", sleep_binary,
                               str(TERMINAL_LIFETIME)]
        yield Scenario(name, layout, focus)


def firefox_window(leaf, firefox_binary, app_id_override, retained_profiles):
    # Keep profiles outside TemporaryDirectory: a disappearing window or exited launcher
    # cannot prove that Firefox's other processes have finished using the directory.
    root = Path(tempfile.mkdtemp(prefix="sway-stage-firefox-"))
    retained_profiles.append(root)
    print(f"      Firefox files retained at {root} (remove manually after all its processes exit)",
          flush=True)
    profile = root / "profile"
    profile.mkdir()
    profile.joinpath("user.js").write_text(
        'user_pref("browser.shell.checkDefaultBrowser", false);\n'
        'user_pref("browser.aboutwelcome.enabled", false);\n'
        'user_pref("browser.startup.homepage_override.mstone", "ignore");\n'
        'user_pref("startup.homepage_welcome_url", "");\n'
        'user_pref("startup.homepage_welcome_url.additional", "");\n', encoding="utf-8")
    child_env = {
        "HOME": str(root), "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_CACHE_HOME": str(root / "cache"), "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "state"), "MOZ_ENABLE_WAYLAND": "1",
        "GDK_BACKEND": "wayland", "XDG_ACTIVATION_TOKEN": "", "DESKTOP_STARTUP_ID": "",
        # Inherited Firefox restart paths must not override our explicit private profile.
        "XRE_PROFILE_PATH": "", "XRE_PROFILE_LOCAL_PATH": "", "XRE_PROFILE_NAME": "",
    }

    app_id = app_id_override
    if app_id is None:
        metadata = Path(firefox_binary).resolve().parent / "application.ini"
        settings = configparser.ConfigParser(interpolation=None)
        settings.read(metadata, encoding="utf-8")
        app_id = settings.get("App", "RemotingName", fallback=None)
        require(app_id, f"cannot determine Firefox Wayland app ID from {metadata}; "
                "supply --firefox-app-id for this installation")
    require(re.fullmatch(r"[A-Za-z][A-Za-z0-9_.-]*", app_id),
            "Firefox app ID must be a simple literal identifier")
    print(f"      Firefox executable={firefox_binary} expected app_id={app_id}", flush=True)
    # --new-instance is also needed: newer Firefox versions ignore --no-remote.
    leaf.update({"match": {"app_id": app_id}, "env": child_env, "check_process": True,
                 "command": [firefox_binary, "--no-remote", "--new-instance", "--profile",
                             str(profile), "--new-window", "about:blank"]})


def xwayland_unavailable():
    if shutil.which("Xwayland") is None or not os.environ.get("DISPLAY"):
        return "Xwayland executable or DISPLAY unavailable"
    # A read-only X query distinguishes a disabled/unreachable X server from an Xterm
    # launch failure. Do not turn a failed application launch into a skip.
    query = shutil.which("xdpyinfo")
    if query is None:
        return "xdpyinfo unavailable (needed for the read-only Xwayland probe)"
    try:
        details = run([query, "-queryExtensions"], "Xwayland availability probe")
    except Failure as error:
        return str(error)
    if "XWAYLAND" not in details:
        return "DISPLAY does not advertise the XWAYLAND extension"
    return ""


def extended_scenarios(run_id, foot_binary, sleep_binary, app_id_override, retained_profiles):
    layouts = [
        Scenario("firefox-two-foot", split("horizontal", [
            window(1, 44), window(2, 32), window(3, 24),
        ]), "window2", FIREFOX_TIMEOUT_MS),
        Scenario("xterm-foot", split("horizontal", [window(1, 3), window(2, 2)]), "window2"),
        Scenario("nested-firefox-foot", split("horizontal", [
            window(1, 44), window(2, 32),
            split("vertical", [window(3, 77), window(4, 23)], 24),
        ]), "window4", FIREFOX_TIMEOUT_MS),
        Scenario("duplicate-foot-ids", split("horizontal", [window(1, 3), window(2, 2)]),
                 "window2"),
    ]
    firefox_binary = shutil.which("firefox-esr") or shutil.which("firefox")
    xterm_binary = shutil.which("xterm")
    for scenario in layouts:
        configured_leaves = list(leaves(scenario.layout))
        for leaf in configured_leaves:
            app_id = f"sway-stage-it-{run_id}-{scenario.name}-{leaf['id']}"
            leaf["match"] = {"app_id": app_id}
            leaf["command"] = [foot_binary, f"--app-id={app_id}", sleep_binary,
                               str(TERMINAL_LIFETIME)]

        if scenario.name != "xterm-foot" and not os.access("/proc/self/cmdline", os.R_OK):
            scenario.skip = "Linux /proc process arguments unavailable for ownership checks"
        elif "firefox" in scenario.name:
            if firefox_binary is None:
                scenario.skip = "neither firefox-esr nor firefox is installed"
            elif not os.environ.get("WAYLAND_DISPLAY"):
                scenario.skip = "native Wayland display unavailable"
            else:
                firefox_window(configured_leaves[0], firefox_binary, app_id_override,
                               retained_profiles)
        elif scenario.name == "xterm-foot":
            scenario.skip = "xterm is not installed" if xterm_binary is None else xwayland_unavailable()
            if not scenario.skip:
                leaf = configured_leaves[0]
                window_class = f"sway-stage-it-{run_id}-xterm"
                # Xterm -class sets WM_CLASS's class, not its instance. -e must be last.
                leaf.update({"match": {"class": window_class},
                             "command": [xterm_binary, "-class", window_class, "+hold", "-e",
                                         sleep_binary, str(TERMINAL_LIFETIME)]})
        else:
            shared_app_id = f"sway-stage-it-{run_id}-duplicate-foot"
            for leaf in configured_leaves:
                # Distinct literal argv supplies independent launch evidence even though
                # both selectors are identical. The title itself is never an ownership key.
                title = f"{shared_app_id}-{leaf['id']}"
                leaf.update({"match": {"app_id": shared_app_id}, "check_process": True,
                             "command": [foot_binary, f"--app-id={shared_app_id}",
                                         f"--title={title}", sleep_binary, str(TERMINAL_LIFETIME)]})
        yield scenario


def yaml_document(layout, focus, shares=None, window_timeout_ms=WINDOW_TIMEOUT_MS):
    """Emit our fixed schema, also serving as the capture format's expected skeleton."""
    draft = shares is not None
    lines = ["version: 1", f"focus: {focus}"]
    if not draft:
        lines.append(f"window_timeout_ms: {window_timeout_ms}")
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
            match_kind, expected_identifier = next(iter(configured["match"].items()))
            fields += [f"command: {command_value}", "match:",
                       f"  {match_kind}: {expected_identifier}"]
            if not draft and "env" in configured:
                fields.append(f"env: {json.dumps(configured['env'])}")
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


def identifier(container, match_kind):
    if match_kind == "class":
        return (container.get("window_properties") or {}).get("class")
    return container.get("app_id")


def outline(container):
    children = container.get("nodes", []) + container.get("floating_nodes", [])
    label = (f"{container.get('id')}:"
             f"{container.get('app_id') or identifier(container, 'class') or container.get('layout')}")
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
        return (container["id"], (container.get("app_id"), identifier(container, "class"),
                                  container.get("pid")))
    events.require_control(container["layout"] in {"splith", "splitv"},
            f"unsupported group: {outline(container)}")
    children = tuple(shape(child, events) for child in container["nodes"])
    if len(children) == 1:
        return children[0]
    return (container["id"], container["layout"], children)


def signature(workspace):
    # Ignore titles/urgency/marks and focus history, but include all geometry, identities,
    # state flags, actual wrappers, and current focus. Capture must not change any of it.
    fields = ("id", "type", "layout", "app_id", "pid", "window", "shell", "rect", "focused",
              "floating", "fullscreen_mode", "scratchpad_state")
    return ({key: workspace.get(key) for key in fields}, identifier(workspace, "class"),
            [signature(child) for child in workspace["nodes"]],
            [signature(child) for child in workspace["floating_nodes"]])


def bind_launches(history, layout, preexisting_ids, events):
    """Associate sequential launches by new-event order, never by selector uniqueness.

    load launches one leaf at a time and snapshots global identities before each spawn.
    We independently require one new event per leaf, then check its eventual tree identity
    and (for Firefox/repeated IDs) private launch argv before accepting cleanup ownership.
    """
    new_ids = []
    for event in history:
        if event.get("change") == "new":
            container_id = event["container"]["id"]
            events.require_control(container_id not in preexisting_ids and container_id not in new_ids,
                    f"preexisting or repeated new event: id={container_id}")
            new_ids.append(container_id)
    configured_leaves = list(leaves(layout))
    events.require_control(len(new_ids) == len(configured_leaves),
            f"expected {len(configured_leaves)} sequential new windows, observed IDs {new_ids}")
    accepted = {container_id: leaf["id"] for container_id, leaf in zip(new_ids, configured_leaves)}
    leaves_by_id = {leaf["id"]: leaf for leaf in configured_leaves}
    for event in history:
        change = event.get("change")
        container = event["container"]
        if change in HARMLESS_EVENTS:
            continue
        container_id = container["id"]
        events.require_control(change in {"new", "focus"} and container_id in accepted,
                f"unexpected activity during load: {change} id={container_id}")
        leaf = leaves_by_id[accepted[container_id]]
        match_kind, expected_identifier = next(iter(leaf["match"].items()))
        observed_identifier = identifier(container, match_kind)
        events.require_control(observed_identifier in (None, expected_identifier),
                f"event identity differs: id={container_id} {match_kind}={observed_identifier!r}, "
                f"expected {expected_identifier!r}")
    return accepted


def require_process_command(container, leaf, events):
    pid = container.get("pid")
    events.require_control(type(pid) is int and pid > 0,
            f"no usable Sway process identity: container={container['id']} leaf={leaf['id']}")
    try:
        arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
    except OSError as error:
        events.require_control(False, f"cannot verify process argv: pid={pid}: {error}")
    # argv[0] may differ after an exec through a distribution wrapper. Every other
    # literal argument must agree, including the private profile or per-leaf Foot title.
    events.require_control(arguments[-1:] == [b""]
            and arguments[1:-1] == [os.fsencode(argument) for argument in leaf["command"][1:]],
            f"process argv does not prove this launch: container={container['id']} "
            f"pid={pid} leaf={leaf['id']}; refusing cleanup")


def ownership(tree, target, layout, preexisting_ids, events, accepted):
    configured_leaves = list(leaves(layout))
    leaves_by_id = {leaf["id"]: leaf for leaf in configured_leaves}
    found = {}
    for container, workspace, unsafe in walk(tree):
        container_id = container["id"]
        on_target = workspace is not None and workspace["id"] == target
        # Existing Firefox windows elsewhere are expected and must remain untouched.
        if container_id in preexisting_ids and not on_target:
            continue
        matches = any(identifier(container, kind) == expected_identifier
                      for leaf in configured_leaves
                      for kind, expected_identifier in leaf["match"].items())
        if container_id not in accepted and not matches and not (on_target and is_window(container)):
            continue
        events.require_control(container_id in accepted and container_id not in preexisting_ids
                and on_target and not unsafe and is_window(container),
                f"ownership/placement uncertain: id={container_id} "
                f"app_id={container.get('app_id')!r} class={identifier(container, 'class')!r} "
                f"workspace={None if workspace is None else workspace['name']!r}")
        leaf = leaves_by_id[accepted[container_id]]
        match_kind, expected_identifier = next(iter(leaf["match"].items()))
        events.require_control(identifier(container, match_kind) == expected_identifier,
                f"owned identity changed: id={container_id} expected {leaf['match']!r}")
        if match_kind == "class":
            events.require_control(container.get("app_id") is None
                    and container.get("window") is not None
                    and container.get("shell") == "xwayland",
                    f"expected an Xwayland window with class: {outline(container)}")
        else:
            events.require_control(container.get("shell") == "xdg_shell"
                    and container.get("window") is None,
                    f"expected a native Wayland window: {outline(container)}")
        if leaf.get("check_process"):
            require_process_command(container, leaf, events)
        found[container_id] = leaf["id"]
    events.require_control(found == accepted,
            f"owned windows changed: expected {accepted!r}, observed {found!r}")
    return found


def verify_layout(workspace, layout, focus, accepted, events):
    shape(workspace, events)  # Validate even wrappers omitted by the comparison below.
    observed_leaves = [container for container, _, _ in walk(workspace) if is_window(container)]
    events.require_control({container["id"] for container in observed_leaves} == set(accepted),
            f"unexpected windows: {outline(workspace)}")
    container_ids = {leaf_id: container_id for container_id, leaf_id in accepted.items()}
    events.require_control([container["id"] for container in observed_leaves]
            == [container_ids[leaf["id"]] for leaf in leaves(layout)],
            f"window order differs: {outline(workspace)}")
    focused = [container for container, _, _ in walk(workspace) if container.get("focused")]
    events.require_control(len(focused) == 1 and focused[0]["id"] == container_ids[focus],
            f"final focus differs: expected {focus} id={container_ids[focus]}, "
            f"observed {outline(workspace)}")
    shares = {}
    diagnostics = []

    def compare(configured, observed, path):
        while not is_window(observed) and len(observed["nodes"]) == 1:
            observed = observed["nodes"][0]
        if "id" in configured:
            expected_id = container_ids[configured["id"]]
            events.require_control(is_window(observed) and observed["id"] == expected_id,
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
        require(max(deviations) <= 5 + 1e-9,
                f"sizing exceeds 5 percentage points (check application minimum sizes): {detail}")
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


def capture_lines(draft):
    return [line.rstrip() for line in draft.splitlines()
            if line.strip() and not line.lstrip().startswith("#")]


def verify_capture(draft, layout, focus, shares, directory, binary):
    # This is a comparison with one known serializer format, not a YAML parser. A format
    # change fails visibly; check supplies authoritative syntax and semantic validation.
    actual = capture_lines(draft)
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
            if key in {"id", "focus", "app_id", "class", "split"}:
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


def verify_inferred_capture(draft, normal_draft, layout):
    # Recognize only the serializer's block command sequences, and compare everything
    # else byte for byte with the already verified normal draft. Do not parse general
    # YAML or include recovered scalars in diagnostics or files.
    lines = capture_lines(draft)
    normalized = []
    commands = []
    line_index = 0
    while line_index < len(lines):
        line = lines[line_index]
        if not line.lstrip().startswith("command:"):
            normalized.append(line)
            line_index += 1
            continue

        indent = len(line) - len(line.lstrip())
        require(line.strip() == "command:", "inferred capture lacks a command array")
        normalized.append(" " * indent + "command: null")
        argument_prefix = " " * (indent + 2) + "- "
        arguments = []
        line_index += 1
        while line_index < len(lines) and lines[line_index].startswith(argument_prefix):
            # Only simple, generated --app-id tokens are interpreted below. Other
            # arguments remain opaque; strict syntax/escaping is covered offline in Rust.
            scalar = lines[line_index][len(argument_prefix):]
            if len(scalar) >= 2 and scalar[0] == scalar[-1] and scalar[0] in "\"'":
                scalar = scalar[1:-1]
            arguments.append(scalar)
            line_index += 1
        require(arguments and arguments[0], "inferred capture has an empty executable")
        commands.append(arguments)

    require(normalized == capture_lines(normal_draft),
            "inference changed capture hierarchy, order, sizing, matchers, IDs, or focus")
    configured_leaves = list(leaves(layout))
    require(len(commands) == len(configured_leaves), "inferred command count differs")
    for arguments, leaf in zip(commands, configured_leaves):
        app_id = leaf["match"]["app_id"]
        paired = any(argument == "--app-id" and arguments[index + 1] == app_id
                     for index, argument in enumerate(arguments[:-1]))
        require(f"--app-id={app_id}" in arguments or paired,
                f"inferred command does not identify the correct Foot window: {leaf['id']}")


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
        expected_remaining = {container_id: leaf_id for container_id, leaf_id in remaining.items()
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


def report_remaining(sway, run_id, known, layout, preexisting_ids):
    print("No further cleanup attempted. Test terminals expire after 300 seconds; "
          "Firefox has no lifetime limit.", file=sys.stderr)
    print(f"  Last associated container IDs -> layout IDs: {known!r}", file=sys.stderr)
    expected_selectors = [leaf["match"] for leaf in leaves(layout)] if layout else []
    try:
        tree = ipc(sway, "get_tree")
        for container, workspace, unsafe in walk(tree):
            app_id = container.get("app_id") or ""
            window_class = identifier(container, "class") or ""
            matches = any(identifier(container, kind) == expected_identifier
                          for matcher in expected_selectors
                          for kind, expected_identifier in matcher.items())
            if (container["id"] in known or matches
                    or (preexisting_ids is not None and container["id"] not in preexisting_ids
                        and is_window(container))
                    or any(value.startswith(f"sway-stage-it-{run_id}-")
                           for value in (app_id, window_class))):
                print(f"  observed id={container['id']} app_id={app_id} "
                      f"class={window_class!r} pid={container.get('pid')} "
                      f"workspace={None if workspace is None else workspace['name']!r} "
                      f"unsafe_state={unsafe} (candidate only; ownership not implied)", file=sys.stderr)
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
    parser.add_argument("--extended", action="store_true",
                        help="also test Firefox, Xterm/Xwayland, and duplicate Foot app IDs")
    parser.add_argument("--firefox-app-id",
                        help="exact native Firefox app ID, overriding application.ini RemotingName")
    parser.add_argument("--binary", default=str(
        Path(__file__).resolve().parents[1] / "target/release/sway-stage"))
    arguments = parser.parse_args()
    events = None
    original = None
    switch_attempted = False
    passed = 0
    failed = 0
    skipped = 0
    stage = "preflight"
    known = {}
    layout = None
    preexisting_ids = None
    retained_profiles = []
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
            for scenario in prepared:
                (directory / f"{scenario.name}.yaml").write_text(
                    yaml_document(scenario.layout, scenario.focus), encoding="utf-8")
                run([binary, "--layouts-dir", temporary, "check", scenario.name],
                    f"check {scenario.name}")
            if arguments.extended:
                prepared = itertools.chain(prepared, extended_scenarios(
                    run_id, executables["foot"], executables["sleep"],
                    arguments.firefox_app_id, retained_profiles))

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

            for scenario in prepared:
                name, layout, focus = scenario.name, scenario.layout, scenario.focus
                stage = f"{name}: empty-workspace precondition"
                known = {}
                tree, _, workspace, history = snapshot(sway, events)
                require_quiet(history, events)
                empty(workspace)
                if scenario.skip:
                    skipped += 1
                    print(f"SKIP  {name:19} {scenario.skip}", flush=True)
                    stage = "scenario preparation"
                    continue
                preexisting_ids = {container["id"] for container, _, _ in walk(tree)}

                layout_path = directory / f"{name}.yaml"
                if not layout_path.exists():
                    stage = f"{name}: check"
                    layout_path.write_text(
                        yaml_document(layout, focus, window_timeout_ms=scenario.window_timeout_ms),
                        encoding="utf-8")
                    run([binary, "--layouts-dir", temporary, "check", name], stage)

                stage = f"{name}: load"
                timeout = len(list(leaves(layout))) * scenario.window_timeout_ms / 1000 + 30
                load_output = run([binary, "--layouts-dir", temporary, "load", name], stage, timeout)
                require(not load_output.strip(), "load produced unexpected stdout")

                stage = f"{name}: raw-tree verification"
                tree, _, workspace, history = snapshot(sway, events)
                known = bind_launches(history, layout, preexisting_ids, events)
                ownership(tree, workspace["id"], layout, preexisting_ids, events, known)
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

                if name in {"single-window", "horizontal-pair"}:
                    stage = f"{name}: inferred capture"
                    inferred_draft = run([binary, "capture", "--infer-commands"], stage, 15)
                    tree, _, workspace, history = snapshot(sway, events)
                    require_quiet(history, events)
                    ownership(tree, workspace["id"], layout, preexisting_ids, events, known)
                    events.require_control(signature(workspace) == before_capture,
                                           "inferred capture changed the workspace")
                    verify_inferred_capture(inferred_draft, draft, layout)

                stage = f"{name}: cleanup"
                cleanup(sway, events, layout, preexisting_ids, known, workspace)
                _, _, workspace, history = snapshot(sway, events)
                require_quiet(history, events)
                empty(workspace)
                passed += 1
                print(f"PASS  {name:19} hierarchy, sizing, focus, identity, capture", flush=True)
                for note in sizing_notes:
                    print(f"      above 1-point sizing target: {note}", flush=True)
                stage = "scenario preparation"
            stage = "final restoration"
            restoration = restore(sway, events, original, arguments.workspace)
            require(restoration == "Original workspace restored.", restoration)
    except (Failure, OSError, ValueError, KeyError, TypeError, StopIteration,
            configparser.Error, KeyboardInterrupt) as error:
        failed = 1
        if isinstance(error, KeyboardInterrupt) and events is not None:
            events.focus_safe = False
        print(f"FAIL  {stage}: {error or 'interrupted'}", file=sys.stderr, flush=True)
        if switch_attempted and sway is not None:
            report_remaining(sway, run_id, known, layout, preexisting_ids)
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
    print(f"\n{passed} passed, {failed} failed, {skipped} skipped")
    for profile_root in retained_profiles:
        print(f"Retained Firefox files: {profile_root}; "
              "remove only after all associated processes exit.")
    if restoration:
        print(restoration)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
