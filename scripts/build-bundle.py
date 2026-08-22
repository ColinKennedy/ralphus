#!/usr/bin/env python3
"""Assemble, zip, checksum, and smoke-test a distributable ralphus bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
import tomllib
import urllib.error
import urllib.request
import zipfile
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build a distributable ralphus bundle from the current checkout."
    )
    parser.add_argument(
        "--allow-dirty",
        action="store_true",
        help="Allow packaging from a dirty git worktree.",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Skip scripts/build-release and package the current dist/ tree as-is.",
    )
    parser.add_argument(
        "--skip-smoke-test",
        action="store_true",
        help="Skip the post-build extract/launch/submit smoke test.",
    )
    parser.add_argument(
        "--keep-stage",
        action="store_true",
        help="Keep the assembled staging directory under the output directory.",
    )
    parser.add_argument(
        "--bundle-name",
        help="Base bundle name (default: ralphus-YYYY-MM-DD).",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("bundle-dist"),
        help="Directory to write the zip, checksum, and optional staging tree into.",
    )
    return parser.parse_args()


def run_checked(
    args: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    text: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=cwd,
        env=env,
        check=True,
        text=text,
        capture_output=True,
    )


def print_step(message: str) -> None:
    print(f"== {message} ==")


def git_output(root: Path, *args: str) -> str:
    return run_checked(["git", *args], cwd=root).stdout.strip()


def dirty_paths(root: Path) -> list[str]:
    output = git_output(root, "status", "--porcelain", "--untracked-files=all")
    paths: list[str] = []
    for line in output.splitlines():
        if not line:
            continue
        entry = line[3:] if len(line) > 3 else line
        if " -> " in entry:
            entry = entry.split(" -> ", 1)[1]
        paths.append(entry)
    return paths


def load_manifest(root: Path) -> dict[str, Any]:
    manifest_path = root / "scripts" / "bundle-manifest.toml"
    with manifest_path.open("rb") as handle:
        return tomllib.load(handle)


def copy_file(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)


def copy_tree(src: Path, dst: Path) -> None:
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest().upper()


def daemon_token_path() -> Path:
    home = os.environ.get("USERPROFILE") or os.environ.get("HOME") or "."
    return Path(home) / ".ralphus" / "daemon.token"


def read_daemon_token() -> str | None:
    token = os.environ.get("RALPHUS_DAEMON_TOKEN", "").strip()
    if token:
        return token
    try:
        return daemon_token_path().read_text(encoding="utf-8").strip() or None
    except OSError:
        return None


def read_version(path: Path) -> str:
    completed = run_checked([str(path), "-V"], cwd=path.parent)
    text = (completed.stdout + completed.stderr).strip()
    return text


def bundle_readme(
    *,
    bundle_name: str,
    commit: str,
    branch: str,
    built_at: str,
    dirty: bool,
    dirty_list: list[str],
    docs: list[str],
) -> str:
    dirty_block = "clean"
    if dirty:
        dirty_block = "dirty\n" + "\n".join(f"  - {entry}" for entry in dirty_list)
    docs_block = "\n".join(f"  - docs/{Path(doc).name}" for doc in docs)
    return textwrap.dedent(
        f"""\
        ralphus deployment bundle
        =========================

        Bundle: {bundle_name}
        Built: {built_at}
        Source commit: {commit}
        Source branch: {branch}
        Source tree: {dirty_block}

        Quick start
        -----------
        1. Run start-ralphus.cmd
           Or, from Git Bash, ./start-ralphus.sh
        2. Open http://127.0.0.1:7474
        3. Validate the example task:
           bin\\ralphus.exe validate examples\\hello.toml
        4. Submit it:
           bin\\ralphus.exe submit examples\\hello.toml --wait

        Bundle layout
        -------------
        - bin/
          ralphus-daemon.exe, ralphus-librarian.exe, ralphus.exe, and ralphus-runner.exe.
        - tmux/
          psmux 3.3.7 plus its LICENSE and upstream README.
        - docs/
          User-facing references copied from this repo:
        {docs_block}
        - examples/
          hello.toml: a no-model smoke-test task that writes hello-from-ralphus.txt in the bundle root.

        Runtime prerequisites
        ---------------------
        - git on PATH
        - model credentials only for prompt/agent sessions; examples/hello.toml needs none
        - write access to the chosen SQLite DB path

        Launcher behavior
        -----------------
        - The launcher exports RALPHUS_RUNNER_CMD, RALPHUS_TMUX_CMD, and RALPHUS_DAEMON_URL before starting the stack.
        - Defaults: daemon on 127.0.0.1:7890, librarian on 127.0.0.1:7474.
        - Pass --daemon-port, --librarian-port, and optionally --db-path to run a second isolated stack.
        - Non-default daemon ports auto-derive a DB at %USERPROFILE%\\.ralphus\\tasks-<port>.db unless --db-path is supplied explicitly.
        - Startup logs go to logs\\daemon.log and logs\\librarian.log inside the extracted bundle directory.

        Troubleshooting
        ---------------
        - If the launcher reports a missing file, keep the extracted folder layout intact; the executables and tmux binary are resolved relative to this folder.
        - If the daemon does not come up, check logs\\daemon.log first, then try bin\\ralphus-daemon.exe serve --port 7890 manually from this folder.
        - If tmux behavior looks wrong, run tmux\\tmux.exe -V and confirm it reports psmux 3.3.7.
        """
    )


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_http(url: str, *, timeout_sec: float, require_daemon_auth: bool = False) -> None:
    deadline = time.time() + timeout_sec
    while time.time() < deadline:
        try:
            request = urllib.request.Request(url)
            if require_daemon_auth:
                token = read_daemon_token()
                if token:
                    request.add_header("Authorization", f"Bearer {token}")
            with urllib.request.urlopen(request, timeout=2) as response:
                if response.status == 200:
                    return
        except urllib.error.HTTPError as exc:
            if exc.code != 401:
                raise
            time.sleep(0.25)
        except urllib.error.URLError:
            time.sleep(0.25)
    raise RuntimeError(f"timed out waiting for {url}")


def windows_kill_tree(pid: int) -> None:
    subprocess.run(
        ["taskkill", "/F", "/T", "/PID", str(pid)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def assemble_bundle(
    *,
    root: Path,
    manifest: dict[str, Any],
    bundle_name: str,
    out_dir: Path,
    allow_dirty: bool,
) -> tuple[Path, dict[str, Any]]:
    bundle_root_name = str(manifest["bundle"]["root_dir"])
    stage_root = out_dir / f"{bundle_name}-stage"
    if stage_root.exists():
        shutil.rmtree(stage_root)
    bundle_root = stage_root / bundle_root_name
    bundle_root.mkdir(parents=True)

    dist_dir = root / "dist"
    copy_file(dist_dir / "ralphus-daemon.exe", bundle_root / "bin" / "ralphus-daemon.exe")
    copy_file(dist_dir / "ralphus-librarian.exe", bundle_root / "bin" / "ralphus-librarian.exe")
    copy_file(dist_dir / "ralphus.exe", bundle_root / "bin" / "ralphus.exe")
    copy_file(dist_dir / "ralphus-runner.exe", bundle_root / "bin" / "ralphus-runner.exe")

    for rel_path in manifest["bundle"]["docs"]:
        src = root / rel_path
        copy_file(src, bundle_root / "docs" / src.name)
    for rel_path in manifest["bundle"]["examples"]:
        src = root / rel_path
        copy_file(src, bundle_root / "examples" / src.name)
    for rel_path in manifest["bundle"]["root_files"]:
        src = root / rel_path
        copy_file(src, bundle_root / src.name)
    for rel_path in manifest["bundle"]["launchers"]:
        src = root / rel_path
        copy_file(src, bundle_root / src.name)

    tmux_manifest = manifest["tmux"]["windows"]
    package_dir = Path(os.path.expandvars(str(tmux_manifest["package_dir"])))
    tmux_exe = package_dir / str(tmux_manifest["exe"])
    tmux_license = package_dir / str(tmux_manifest["license"])
    tmux_readme = package_dir / str(tmux_manifest["readme"])
    for required in (tmux_exe, tmux_license, tmux_readme):
        if not required.is_file():
            raise FileNotFoundError(f"required tmux asset missing: {required}")
    actual_hash = sha256_file(tmux_exe)
    expected_hash = str(tmux_manifest["sha256"]).upper()
    if actual_hash != expected_hash:
        raise RuntimeError(
            f"tmux checksum mismatch: expected {expected_hash}, found {actual_hash}"
        )
    version_text = read_version(tmux_exe)
    expected_version = str(tmux_manifest["version_substring"])
    if expected_version not in version_text:
        raise RuntimeError(
            f"tmux version mismatch: expected substring {expected_version!r}, got {version_text!r}"
        )
    copy_file(tmux_exe, bundle_root / "tmux" / "tmux.exe")
    copy_file(tmux_license, bundle_root / "tmux" / "LICENSE")
    copy_file(tmux_readme, bundle_root / "tmux" / "psmux-README.md")

    commit = git_output(root, "rev-parse", "HEAD")
    branch = git_output(root, "rev-parse", "--abbrev-ref", "HEAD")
    dirty_list = dirty_paths(root)
    built_at = time.strftime("%Y-%m-%d %H:%M:%S %z")
    provenance = {
        "bundle_name": bundle_name,
        "built_at": built_at,
        "source_commit": commit,
        "source_branch": branch,
        "source_dirty": bool(dirty_list),
        "source_dirty_paths": dirty_list,
        "allow_dirty": allow_dirty,
        "tmux": {
            "path": str(tmux_exe),
            "sha256": actual_hash,
            "version": version_text,
        },
        "manifest": manifest,
    }
    readme = bundle_readme(
        bundle_name=bundle_name,
        commit=commit,
        branch=branch,
        built_at=built_at,
        dirty=bool(dirty_list),
        dirty_list=dirty_list,
        docs=list(manifest["bundle"]["docs"]),
    )
    (bundle_root / "README.txt").write_text(readme, encoding="utf-8")
    (bundle_root / "PROVENANCE.json").write_text(
        json.dumps(provenance, indent=2) + "\n",
        encoding="utf-8",
    )
    copy_file(root / "scripts" / "bundle-manifest.toml", bundle_root / "BUNDLE-MANIFEST.toml")
    return stage_root, provenance


def zip_bundle(stage_root: Path, bundle_root_name: str, zip_path: Path) -> None:
    if zip_path.exists():
        zip_path.unlink()
    with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        root_dir = stage_root / bundle_root_name
        for path in sorted(root_dir.rglob("*")):
            zf.write(path, path.relative_to(stage_root))


def smoke_test(zip_path: Path) -> None:
    print_step("smoke test")
    with tempfile.TemporaryDirectory(prefix="ralphus-bundle-smoke-") as tmp:
        temp_root = Path(tmp)
        with zipfile.ZipFile(zip_path) as zf:
            zf.extractall(temp_root)
        bundle_root = temp_root / "ralphus"
        daemon_port = free_port()
        librarian_port = free_port()
        db_path = temp_root / "smoke.db"
        launcher = bundle_root / "start-ralphus.cmd"
        env = os.environ.copy()
        launcher_proc = subprocess.Popen(
            [
                "cmd",
                "/c",
                str(launcher),
                "--daemon-port",
                str(daemon_port),
                "--librarian-port",
                str(librarian_port),
                "--db-path",
                str(db_path),
            ],
            cwd=bundle_root,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_http(
                f"http://127.0.0.1:{daemon_port}/api/daemon",
                timeout_sec=30.0,
                require_daemon_auth=True,
            )
            wait_http(f"http://127.0.0.1:{librarian_port}/", timeout_sec=30.0)

            tmux_version = read_version(bundle_root / "tmux" / "tmux.exe")
            if "psmux 3.3.7" not in tmux_version:
                raise RuntimeError(f"unexpected bundled tmux version: {tmux_version}")

            cli = bundle_root / "bin" / "ralphus.exe"
            daemon_url = f"http://127.0.0.1:{daemon_port}"
            run_checked(
                [str(cli), "--daemon-url", daemon_url, "validate", "examples/hello.toml"],
                cwd=bundle_root,
            )
            submit = run_checked(
                [str(cli), "--daemon-url", daemon_url, "--json", "submit", "examples/hello.toml"],
                cwd=bundle_root,
            )
            result = json.loads(submit.stdout)
            squad_id = str(result["squad_id"])
            deadline = time.time() + 60.0
            final_state = ""
            while time.time() < deadline:
                show = run_checked(
                    [str(cli), "--daemon-url", daemon_url, "--json", "squad", "show", squad_id],
                    cwd=bundle_root,
                )
                run_view = json.loads(show.stdout)
                final_state = str(run_view["state"])
                if final_state in {"done", "failed", "cancelled"}:
                    break
                time.sleep(1.0)
            if final_state != "done":
                raise RuntimeError(
                    f"bundle smoke squad {squad_id} ended in state {final_state!r}"
                )
            hello_file = bundle_root / "hello-from-ralphus.txt"
            if hello_file.read_text(encoding="utf-8").strip() != "hello from ralphus":
                raise RuntimeError("bundle smoke run did not write the expected output file")
        finally:
            daemon_exe = bundle_root / "bin" / "ralphus-daemon.exe"
            subprocess.run(
                [str(daemon_exe), "stop", "--port", str(daemon_port)],
                cwd=bundle_root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            time.sleep(1.0)
            if launcher_proc.poll() is None:
                windows_kill_tree(launcher_proc.pid)


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    manifest = load_manifest(root)
    dirty_list = dirty_paths(root)
    if dirty_list and not args.allow_dirty:
        print("error: git worktree is dirty; re-run with --allow-dirty to package anyway", file=sys.stderr)
        for entry in dirty_list:
            print(f"  {entry}", file=sys.stderr)
        return 1

    bundle_name = args.bundle_name or f"ralphus-{time.strftime('%Y-%m-%d')}"
    out_dir = (root / args.output_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    if not args.skip_build:
        print_step("build release")
        build_script = root / "scripts" / "build-release.cmd"
        subprocess.run(["cmd", "/c", str(build_script)], cwd=root, check=True)

    print_step("assemble bundle")
    stage_root, _provenance = assemble_bundle(
        root=root,
        manifest=manifest,
        bundle_name=bundle_name,
        out_dir=out_dir,
        allow_dirty=args.allow_dirty,
    )
    zip_path = out_dir / f"{bundle_name}.zip"
    zip_bundle(stage_root, str(manifest["bundle"]["root_dir"]), zip_path)
    checksum = sha256_file(zip_path)
    checksum_path = out_dir / f"{bundle_name}.sha256"
    checksum_path.write_text(f"{checksum}  {zip_path.name}\n", encoding="ascii")

    if not args.skip_smoke_test:
        smoke_test(zip_path)

    if not args.keep_stage:
        shutil.rmtree(stage_root)

    print_step("done")
    print(zip_path)
    print(checksum_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
