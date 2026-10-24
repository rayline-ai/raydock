#!/usr/bin/env python3
"""Validate three-binary install integrity and same-version core repair detection.

Uses built Rust binaries and the external core contract fixture. No inference,
network service, release upload, or user installation is touched. The staged
installer path intentionally exercises the existing no-minisign mode; Rust
signature-verification tests cover the signed updater boundary.
"""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    with tempfile.TemporaryDirectory(prefix="rayline-release-install-") as directory:
        root = Path(directory)
        assets, installed, commands = root / "assets", root / "installed", root / "commands"
        for path in (assets, commands):
            path.mkdir()
        # A controlled command path makes the unsigned staging scenario explicit,
        # independent of whether the developer has minisign installed globally.
        for name in ("sh", "cat", "curl", "awk", "uname", "mktemp", "rm", "shasum", "install", "mkdir"):
            executable = shutil.which(name)
            assert executable, f"required test command: {name}"
            (commands / name).symlink_to(executable)
        system, arch = os.uname().sysname, os.uname().machine
        platform = {("Darwin", "arm64"): "macosx_11_0_arm64", ("Darwin", "x86_64"): "macosx_10_12_x86_64",
                    ("Linux", "x86_64"): "linux_x86_64", ("Linux", "aarch64"): "linux_aarch64"}[(system, arch)]
        files = {"rayline": ROOT / "target/debug/rayline", "rld": ROOT / "target/debug/rld",
                 "rayline-core": ROOT / "crates/rayline-daemon/tests/fixtures/execution-contract.py"}
        expected = {}
        for name, source in files.items():
            expected[name] = source.read_bytes()
            (assets / f"{name}-{platform}").write_bytes(expected[name])
        sums = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}-{platform}\n" for name, data in expected.items())
        (assets / "SHA256SUMS").write_text(sums)
        env = dict(os.environ, PATH=str(commands), HOME=str(root), RAYLINE_RELEASE_BASE_URL=assets.as_uri())

        def install(success):
            result = subprocess.run([str(commands / "sh"), str(ROOT / "scripts/install-rayline.sh"),
                                     "--install-dir", str(installed)], env=env, capture_output=True, text=True)
            assert (result.returncode == 0) == success, result.stdout + result.stderr

        install(True)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
        version = subprocess.check_output([str(installed / "rayline"), "--version"], text=True).split()[1]
        check = [str(installed / "rayline"), "update", "--check", "--channel", "prod", "--version", version]
        check_env = dict(os.environ, HOME=str(root), RAYLINE_UPDATE_INSTALL_PATH=str(installed / "rayline"))
        check_env.pop("RAYLINE_UPDATE_CHANNEL", None)
        complete = subprocess.run(check, env=check_env, capture_output=True, text=True)
        assert complete.returncode == 0, complete.stdout + complete.stderr
        (installed / "rayline-core").unlink()
        repair = subprocess.run(check, env=check_env, capture_output=True, text=True)
        assert repair.returncode == 1 and "Execution core is missing" in repair.stdout, repair.stdout + repair.stderr
        (installed / "rayline-core").write_bytes(expected["rayline-core"])
        (assets / f"rayline-core-{platform}").write_bytes(b"tampered")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data, "failed verification changed installed files"
        (assets / f"rayline-core-{platform}").write_bytes(expected["rayline-core"])
        (assets / "SHA256SUMS").write_text("\n".join(line for line in sums.splitlines() if "rayline-core" not in line) + "\n")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
    print("Release installation passed: three binaries, missing-core repair detection, tamper and missing-checksum refusal.")


if __name__ == "__main__":
    main()
