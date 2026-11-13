#!/usr/bin/env python3
"""Validate rayline and core install integrity.

Uses built Rust binaries and the external core contract fixture. No inference,
network service, release upload, or user installation is touched. The installer
runs with a PATH that has no minisign: it checks the release with SHA256SUMS only.
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
        # A controlled command path, without minisign.
        for name in ("sh", "cat", "curl", "awk", "uname", "mktemp", "rm", "shasum", "install", "mkdir"):
            executable = shutil.which(name)
            assert executable, f"required test command: {name}"
            (commands / name).symlink_to(executable)
        installer = ROOT / "scripts/install-rayline.sh"

        def write_sums(text):
            (assets / "SHA256SUMS").write_text(text)
        system, arch = os.uname().sysname, os.uname().machine
        platform = {("Darwin", "arm64"): "macosx_11_0_arm64", ("Darwin", "x86_64"): "macosx_10_12_x86_64",
                    ("Linux", "x86_64"): "linux_x86_64", ("Linux", "aarch64"): "linux_aarch64"}[(system, arch)]
        files = {"rayline": ROOT / "target/debug/rayline",
                 "rayline-core": ROOT / "crates/rayline-cli/tests/fixtures/execution-contract.py"}
        expected = {}
        for name, source in files.items():
            expected[name] = source.read_bytes()
            (assets / f"{name}-{platform}").write_bytes(expected[name])
        sums = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}-{platform}\n" for name, data in expected.items())
        license_data = (ROOT / "LICENSE").read_bytes()
        (assets / "rayline-core-LICENSE").write_bytes(license_data)
        sums += f"{hashlib.sha256(license_data).hexdigest()}  rayline-core-LICENSE\n"
        write_sums(sums)

        def run_installer(*args, **env):
            env = dict(os.environ, PATH=str(commands), HOME=str(root), **env)
            return subprocess.run([str(commands / "sh"), str(installer), *args], env=env, capture_output=True, text=True)

        def install(success):
            result = run_installer("--base-url", assets.as_uri(), "--install-dir", str(installed))
            assert (result.returncode == 0) == success, result.stdout + result.stderr

        # --base-url names one release, so --version can't be combined with it.
        both = run_installer("--base-url", assets.as_uri(), "--version", "0.2.0", "--install-dir", str(installed))
        assert both.returncode == 2 and "--base-url" in both.stderr, both.stdout + both.stderr
        # --version takes a bare version, the spelling `rayline update --version` takes.
        prefixed = run_installer("--version", "v0.2.0", "--install-dir", str(installed))
        assert prefixed.returncode == 2 and "--version" in prefixed.stderr, prefixed.stdout + prefixed.stderr
        # Only "<sha256>  <name>" lines are entries; a binary-mode "*<name>" line is not.
        write_sums(sums.replace(f"  rayline-{platform}\n", f" *rayline-{platform}\n"))
        install(False)
        write_sums(sums)
        assert not installed.exists(), "a refused install wrote files"
        # The install dir comes from --install-dir only; RAYLINE_INSTALL_DIR is ignored.
        ignored = root / "ignored"
        default = run_installer("--base-url", assets.as_uri(), RAYLINE_INSTALL_DIR=str(ignored))
        assert default.returncode == 0, default.stdout + default.stderr
        assert (root / ".rayline/bin/rayline").read_bytes() == expected["rayline"]
        assert not ignored.exists()

        install(True)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
        assert (installed / "rayline-core-LICENSE").read_bytes() == license_data
        assert sorted(path.name for path in installed.iterdir()) == ["rayline", "rayline-core", "rayline-core-LICENSE"]
        (assets / "rayline-core-LICENSE").write_bytes(b"tampered license")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data, "license verification changed installed files"
        assert (installed / "rayline-core-LICENSE").read_bytes() == license_data
        (assets / "rayline-core-LICENSE").write_bytes(license_data)
        version = subprocess.check_output([str(installed / "rayline"), "--version"], text=True).split()[1]
        check = [str(installed / "rayline"), "update", "--check", "--version", version, "--install-path", str(installed / "rayline")]
        check_env = {key: value for key, value in os.environ.items() if not key.startswith("RAYLINE_")}
        check_env["HOME"] = str(root)
        complete = subprocess.run(check, env=check_env, capture_output=True, text=True)
        assert complete.returncode == 0, complete.stdout + complete.stderr
        (assets / f"rayline-core-{platform}").write_bytes(b"tampered")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data, "failed verification changed installed files"
        (assets / f"rayline-core-{platform}").write_bytes(expected["rayline-core"])
        write_sums("\n".join(line for line in sums.splitlines() if "rayline-core" not in line) + "\n")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
    print("Release installation passed: Rayline CLI and core without minisign, "
          "--base-url/--version and v-prefixed --version refusal, tamper, binary-mode and missing-checksum refusal.")


if __name__ == "__main__":
    main()
