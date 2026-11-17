#!/usr/bin/env python3
"""CI-only pinned Sway builds; never starts a compositor or connects to IPC.

The build-sway composite action installs dependencies, prepares the input manifest,
restores an exact cache, builds on a miss, and validates on both paths. Run with
--help for the individual phases. Requires the selected Ubuntu 26.04 runner.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
# (tag, annotated tag object, peeled commit), always fetched from canonical origins.
SOURCES = {
    "1.10": {
        "sway": ("1.10", "014b1b025e964262a88fc46071291288abeba7f3",
                 "c93ff73051682913bf0238d15d6ee25d9a3e1a08"),
        "wlroots": ("0.18.2", "e8e95d1bb01783c064eea998e9f8965731d67a47",
                    "cda69b696d65a53d5d5e75dfed059a3803e0d700"),
    },
    "1.11": {
        "sway": ("1.11", "77c4b65958c9c4403b0e15ac1ddae2731609183b",
                 "990b23c0e584114c3694447344d6bbf44b2d748f"),
        "wlroots": ("0.19.3", "5b050dbb9764c3724b12859e8d44d6b07c93b135",
                    "88a869855742281c98c22cab9641b317b8d065ef"),
    },
    "1.12": {
        "sway": ("1.12", "45961113734a4d2cd3a652723c7499aa5d2e19b6",
                 "88869399f421d9180dd8b6ed0b5a1f4a3585d252"),
        "wlroots": ("0.20.2", "7621df59d572696a09979cff03615fa51c893f24",
                    "d783533489e1f75d6886c2ab5c5960090ef268f8"),
    },
}
ORIGINS = {
    "sway": "https://github.com/swaywm/sway.git",
    "wlroots": "https://gitlab.freedesktop.org/wlroots/wlroots.git",
}
COMMON_OPTIONS = [
    "--buildtype=release", "--wrap-mode=nofallback", "--libdir=lib",
    "-Ddefault_library=shared", "-Dc_std=c11", "-Dwerror=true",
]
PROJECT_OPTIONS = {
    "wlroots": [
        "-Dauto_features=disabled", "-Dexamples=false", "-Dbackends=[]",
        "-Drenderers=[]", "-Dallocators=[]", "-Dxwayland=disabled",
        "-Dxcb-errors=disabled", "-Dsession=disabled", "-Dcolor-management=disabled",
        "-Dlibliftoff=disabled",
    ],
    "sway": [
        "-Dauto_features=disabled", "-Dswaybar=false", "-Dswaynag=false",
        "-Dtray=disabled", "-Dgdk-pixbuf=disabled", "-Dman-pages=disabled",
        "-Ddefault-wallpaper=false", "-Dzsh-completions=false",
        "-Dbash-completions=false", "-Dfish-completions=false",
    ],
}
# Check the strictest pair's requirements for every entry, keeping provisioning uniform.
DEPENDENCIES = {
    "wayland-server": "1.24.0", "wayland-client": "1.24.0",
    "wayland-cursor": "1.24.0", "wayland-scanner": "1.24.0",
    "wayland-protocols": "1.47", "libdrm": "2.4.129", "pixman-1": "0.46.0",
    "xkbcommon": "1.8.0", "json-c": "0.13", "libpcre2-8": "",
    "cairo": "", "pango": "", "pangocairo": "", "libevdev": "",
}
SYSTEM_ENV = {"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"}


def run(arguments, *, cwd=None, env=None, capture=True, timeout=120):
    print(f"+ {shlex.join(str(argument) for argument in arguments)}", flush=True)
    try:
        result = subprocess.run(
            arguments, cwd=cwd, env=SYSTEM_ENV if env is None else env,
            check=True, text=True, stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.STDOUT if capture else None,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError) as error:
        details = getattr(error, "stdout", None) or ""
        raise RuntimeError(
            f"command failed: {shlex.join(map(str, arguments))}: {error}\n{details}"
        ) from error
    return result.stdout.strip() if capture else None


def serialized(inputs):
    return json.dumps(inputs, sort_keys=True, indent=2) + "\n"


def cache_key(inputs):
    digest = hashlib.sha256(serialized(inputs).encode()).hexdigest()
    pins = inputs["sources"]
    return (f"sway-ci-v1-ubuntu26.04-{inputs['architecture']}-"
            f"{pins['sway'][2]}-{pins['wlroots'][2]}-{digest}")


def collect_inputs(sway_tag):
    os_release = Path("/etc/os-release").read_text()
    release_fields = dict(line.split("=", 1) for line in os_release.splitlines() if "=" in line)
    if release_fields.get("ID") != "ubuntu" or release_fields.get("VERSION_ID") != '"26.04"':
        raise RuntimeError("build-sway requires Ubuntu 26.04")

    architecture = run(["dpkg", "--print-architecture"])
    if architecture != "amd64":
        raise RuntimeError(f"build-sway requires amd64, found {architecture}")
    runner_image = {name: os.environ[name] for name in ("ImageOS", "ImageVersion")}
    home = Path(os.environ["HOME"])
    if not home.is_absolute():
        raise RuntimeError("HOME must be absolute")
    prefix = home / ".cache/sway-stage-ci" / f"sway-{sway_tag}"

    tools = {}
    for name in ("gcc-13", "ld", "git", "meson", "ninja", "pkg-config", "python3",
                 "readelf", "ldd", "wayland-scanner", "foot"):
        executable = shutil.which(name, path=SYSTEM_ENV["PATH"])
        if executable is None:
            raise RuntimeError(f"missing prerequisite: {name}; provision the CI packages first")
        tools[name] = {"path": str(Path(executable).resolve()),
                       "version": run([executable, "--version"])}
    if tuple(map(int, tools["meson"]["version"].split("."))) < (1, 3):
        raise RuntimeError("Meson >=1.3 is required")
    foot_package = run(["dpkg-query", "-W", "-f=${Version}", "foot"])
    if foot_package != "1.25.0-1":
        raise RuntimeError(f"expected foot=1.25.0-1, found {foot_package}")

    dependencies = {}
    for name, minimum in DEPENDENCIES.items():
        if minimum:
            run(["pkg-config", f"--atleast-version={minimum}", name])
        dependencies[name] = {
            "version": run(["pkg-config", "--modversion", name]),
            "pcfiledir": run(["pkg-config", "--variable=pcfiledir", name]),
            "prefix": run(["pkg-config", "--variable=prefix", name]),
        }

    package_format = "${binary:Package}\t${Version}\t${Architecture}\t${db:Status-Status}\n"
    packages = run(["dpkg-query", "-W", f"-f={package_format}"])
    return {
        "sway_tag": sway_tag, "sources": SOURCES[sway_tag], "origins": ORIGINS,
        "common_options": COMMON_OPTIONS, "project_options": PROJECT_OPTIONS,
        "prefix": str(prefix), "architecture": architecture, "os_release": os_release,
        "runner_image": runner_image, "tools": tools, "dependencies": dependencies,
        "packages": sorted(packages.splitlines()),
        "implementation_sha256": {
            str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (Path(__file__).resolve(), ROOT / ".github/actions/build-sway/action.yml")
        },
    }


def fetch_source(directory, project, pin):
    tag, tag_object, commit = pin
    run(["git", "init", directory])
    run(["git", "remote", "add", "origin", ORIGINS[project]], cwd=directory)
    run(["git", "fetch", "--depth=1", "--no-tags", "origin",
         f"refs/tags/{tag}:refs/tags/{tag}"], cwd=directory, timeout=300)

    for revision, expected in ((f"refs/tags/{tag}", tag_object),
                               (f"refs/tags/{tag}^{{commit}}", commit)):
        actual = run(["git", "rev-parse", "--verify", revision], cwd=directory)
        if actual != expected:
            raise RuntimeError(f"{project} {revision}: expected {expected}, fetched {actual}")
    run(["git", "checkout", "--detach", commit], cwd=directory)


def build(inputs):
    prefix = Path(inputs["prefix"])
    if prefix.exists():
        raise RuntimeError(
            f"cold build requires an absent prefix: {prefix}; refusing to mix installs"
        )
    build_env = dict(
        SYSTEM_ENV, CC="/usr/bin/gcc-13", PKG_CONFIG_PATH=str(prefix / "lib/pkgconfig")
    )
    options = inputs["common_options"] + [
        f"--prefix={prefix}", f"-Dc_link_args=-Wl,-rpath,{prefix}/lib",
    ]

    with tempfile.TemporaryDirectory(prefix="sway-stage-ci-build-") as temporary:
        for project in ("wlroots", "sway"):
            source = Path(temporary) / project
            fetch_source(source, project, inputs["sources"][project])
            if project == "sway":
                verify_private_wlroots(inputs, build_env)

            build_directory = source / "build"
            run(["meson", "setup", build_directory, source, *options,
                 *inputs["project_options"][project]], env=build_env, capture=False)
            run(["meson", "compile", "-C", build_directory], env=build_env,
                capture=False, timeout=1200)
            run(["meson", "install", "--no-rebuild", "-C", build_directory],
                env=build_env, capture=False)

    (prefix / "build-inputs.json").write_text(serialized(inputs))


def verify_private_wlroots(inputs, env):
    abi = ".".join(inputs["sources"]["wlroots"][0].split(".")[:2])
    package = f"wlroots-{abi}"
    directory = run(["pkg-config", "--variable=pcfiledir", package], env=env)
    expected = Path(inputs["prefix"]) / "lib/pkgconfig"
    if Path(directory).resolve() != expected.resolve():
        raise RuntimeError(f"{package} resolved outside the private prefix: {directory}")
    version = run(["pkg-config", "--modversion", package], env=env)
    if version != inputs["sources"]["wlroots"][0]:
        raise RuntimeError(f"{package}: unexpected installed version {version}")
    return abi


def validate(inputs):
    prefix = Path(inputs["prefix"])
    if (prefix / "build-inputs.json").read_text() != serialized(inputs):
        raise RuntimeError("cached build-inputs.json differs from this job's prepared inputs")
    # Deliberately no HOME, display, IPC, or library variables, like env -i PATH=... .
    runtime_env = {"PATH": f"{prefix}/bin:/usr/bin:/bin"}
    pkg_config_env = dict(runtime_env, PKG_CONFIG_PATH=str(prefix / "lib/pkgconfig"))
    abi = verify_private_wlroots(inputs, pkg_config_env)

    for name in ("sway", "swaymsg"):
        executable = shutil.which(name, path=runtime_env["PATH"])
        expected = prefix / "bin" / name
        if executable is None or Path(executable).resolve() != expected.resolve():
            raise RuntimeError(f"{name} does not resolve through {expected}: {executable}")
        if not Path(executable).resolve().is_relative_to(prefix.resolve()):
            raise RuntimeError(f"{name} resolves outside the private prefix: {executable}")
        print(f"{name}: {executable} -> {Path(executable).resolve()}", flush=True)
        version = run([name, "--version"], env=runtime_env)
        print(version, flush=True)
        if version != f"{name} version {inputs['sway_tag']}":
            raise RuntimeError(f"unexpected {name} version: {version}")

        dynamic = run(["readelf", "-d", executable], env=runtime_env)
        print(dynamic, flush=True)
        search_paths = re.findall(r"\((?:RUNPATH|RPATH)\).*?\[(.*?)\]", dynamic)
        if str(prefix / "lib") not in [path for paths in search_paths for path in paths.split(":")]:
            raise RuntimeError(f"{name} has no embedded private library search path")
        libraries = run(["ldd", executable], env=runtime_env)
        print(libraries, flush=True)
        if "not found" in libraries:
            raise RuntimeError(f"{name} has missing runtime libraries")
        if name == "sway":
            library = re.search(rf"libwlroots-{re.escape(abi)}\.so\S* => (\S+)", libraries)
            private_library_directory = (prefix / "lib").resolve()
            if library is None or not Path(library[1]).resolve().is_relative_to(
                private_library_directory
            ):
                raise RuntimeError("Sway did not load its private wlroots library")
    print(f"Validated {inputs['sway_tag']}: {cache_key(inputs)}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("prepare", "build", "validate"))
    parser.add_argument("sway_tag", choices=SOURCES)
    parser.add_argument("manifest", type=Path, help="current job's inputs, outside the cached prefix")
    arguments = parser.parse_args()

    # Recollect rather than trust a stale manifest or changed dependencies between phases.
    inputs = collect_inputs(arguments.sway_tag)
    contents = serialized(inputs)
    print(contents, flush=True)
    if arguments.phase == "prepare":
        arguments.manifest.write_text(contents)
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"prefix={inputs['prefix']}\ncache-key={cache_key(inputs)}\n")
    else:
        if arguments.manifest.read_text() != contents:
            raise RuntimeError("prepared inputs changed; rerun preparation before using the cache")
        if arguments.phase == "build":
            build(inputs)
        else:
            validate(inputs)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"build-sway: {error}", file=sys.stderr)
        sys.exit(1)
