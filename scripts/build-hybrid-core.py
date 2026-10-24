#!/usr/bin/env python3
"""Build a release core from the exact, clean public source and Go toolchain."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "macosx_10_12_x86_64": ("darwin", "amd64"),
    "macosx_11_0_arm64": ("darwin", "arm64"),
    "linux_x86_64": ("linux", "amd64"),
    "linux_aarch64": ("linux", "arm64"),
    "windows_x86_64": ("windows", "amd64"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--platform", choices=TARGETS, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    manifest_path = ROOT / "release/core-source.json"
    manifest = json.loads(manifest_path.read_text())
    source = args.source_dir.resolve()
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    if commit != manifest["commit"]:
        parser.error("core source HEAD does not match release/core-source.json")
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=source, text=True).strip():
        parser.error("core source checkout must be clean, including untracked files")
    version = subprocess.check_output(["go", "env", "GOVERSION"], text=True).strip()
    if version != "go" + manifest["go_version"]:
        parser.error("Go toolchain does not match release/core-source.json")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    artifact = args.output_dir.resolve() / ("rayline-core-" + args.platform)
    goos, goarch = TARGETS[args.platform]
    env = dict(os.environ, CGO_ENABLED="0", GOOS=goos, GOARCH=goarch, GOTOOLCHAIN="local", GOWORK="off", GOAMD64="v1", GOARM64="v8.0")
    subprocess.run(["go", "build", "-mod=readonly", "-trimpath", "-buildvcs=true", "-o", str(artifact), "./cmd/rayline-core"],
                   cwd=source / "rayline-core", env=env, check=True)
    metadata = subprocess.check_output(["go", "version", "-m", str(artifact)], text=True)
    if "vcs.revision=" + commit not in metadata or "vcs.modified=false" not in metadata:
        artifact.unlink()
        parser.error("built execution core is missing clean pinned VCS provenance")
    shutil.copyfile(manifest_path, args.output_dir / "rayline-core-source.json")
    print(artifact)


if __name__ == "__main__":
    main()
