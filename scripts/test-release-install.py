#!/usr/bin/env python3
"""Validate raydock and core install integrity and same-version core repair detection.

Uses built Rust binaries and the external core contract fixture. No inference,
network service, release upload, or user installation is touched. The staged
release is signed with a throwaway minisign key, and the installer under test is
a copy of install-rayline.sh with that key in place of the pinned release key.
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
        no_minisign = root / "no-minisign"
        for path in (assets, commands, no_minisign):
            path.mkdir()
        # Controlled command paths: one with minisign, one without.
        for name in ("sh", "cat", "curl", "awk", "uname", "mktemp", "rm", "shasum", "install", "mkdir", "ln"):
            executable = shutil.which(name)
            assert executable, f"required test command: {name}"
            (commands / name).symlink_to(executable)
            (no_minisign / name).symlink_to(executable)
        minisign = shutil.which("minisign")
        assert minisign, "required test command: minisign"
        (commands / "minisign").symlink_to(minisign)
        public_key_file, secret_key_file = root / "test.pub", root / "test.key"
        subprocess.run([minisign, "-G", "-W", "-p", str(public_key_file), "-s", str(secret_key_file)],
                       check=True, capture_output=True)
        test_key = public_key_file.read_text().splitlines()[1]
        real_installer = ROOT / "scripts/install-rayline.sh"
        installer_text = real_installer.read_text()
        pinned_key = "RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1"
        assert installer_text.count(pinned_key) == 1, "the installer pins exactly one release key"
        installer = root / "install-rayline.sh"
        installer.write_text(installer_text.replace(pinned_key, test_key))

        def write_sums(text, sign=True):
            (assets / "SHA256SUMS").write_text(text)
            if sign:
                subprocess.run([minisign, "-S", "-s", str(secret_key_file), "-m", str(assets / "SHA256SUMS")],
                               check=True, capture_output=True)
        system, arch = os.uname().sysname, os.uname().machine
        platform = {("Darwin", "arm64"): "macosx_11_0_arm64", ("Darwin", "x86_64"): "macosx_10_12_x86_64",
                    ("Linux", "x86_64"): "linux_x86_64", ("Linux", "aarch64"): "linux_aarch64"}[(system, arch)]
        files = {"rayline": ROOT / "target/debug/raydock",
                 "rayline-core": ROOT / "crates/rayline-daemon/tests/fixtures/execution-contract.py"}
        expected = {}
        for name, source in files.items():
            expected[name] = source.read_bytes()
            (assets / f"{name}-{platform}").write_bytes(expected[name])
        sums = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}-{platform}\n" for name, data in expected.items())
        license_data = (ROOT / "LICENSE").read_bytes()
        (assets / "rayline-core-LICENSE").write_bytes(license_data)
        sums += f"{hashlib.sha256(license_data).hexdigest()}  rayline-core-LICENSE\n"
        write_sums(sums)

        def run_installer(*args, script=installer, path=commands, **env):
            env = dict(os.environ, PATH=str(path), HOME=str(root), **env)
            return subprocess.run([str(commands / "sh"), str(script), *args], env=env, capture_output=True, text=True)

        def install(success):
            result = run_installer("--base-url", assets.as_uri(), "--install-dir", str(installed))
            assert (result.returncode == 0) == success, result.stdout + result.stderr

        # minisign is required: without it nothing is downloaded or installed.
        missing = run_installer("--base-url", assets.as_uri(), "--install-dir", str(installed), path=no_minisign)
        assert missing.returncode == 1 and "minisign" in missing.stderr, missing.stdout + missing.stderr
        # --base-url names one release, so --version can't be combined with it.
        both = run_installer("--base-url", assets.as_uri(), "--version", "0.2.0", "--install-dir", str(installed))
        assert both.returncode == 2 and "--base-url" in both.stderr, both.stdout + both.stderr
        # The real installer trusts only the pinned key; the old key override is gone.
        foreign = run_installer("--base-url", assets.as_uri(), "--install-dir", str(installed),
                                script=real_installer, RAYLINE_MINISIGN_PUBKEY=test_key)
        assert foreign.returncode == 1 and "signature verification failed" in foreign.stderr, \
            foreign.stdout + foreign.stderr
        # An unsigned change to SHA256SUMS fails the signature check.
        write_sums(sums + f"{'0' * 64}  extra\n", sign=False)
        install(False)
        write_sums(sums)
        assert not installed.exists(), "a refused install wrote files"
        # The install dir comes from --install-dir only; RAYLINE_INSTALL_DIR is ignored.
        ignored = root / "ignored"
        default = run_installer("--base-url", assets.as_uri(), RAYLINE_INSTALL_DIR=str(ignored))
        assert default.returncode == 0, default.stdout + default.stderr
        assert (root / ".rayline/bin/raydock").read_bytes() == expected["rayline"]
        assert not ignored.exists()

        install(True)
        assert (installed / "rayline").is_symlink()
        assert (installed / "rayline").resolve() == (installed / "raydock").resolve()
        assert (installed / "raydock").read_bytes() == expected["rayline"]
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
        assert (installed / "rayline-core-LICENSE").read_bytes() == license_data
        assert sorted(path.name for path in installed.iterdir()) == ["raydock", "rayline", "rayline-core", "rayline-core-LICENSE"]
        (assets / "rayline-core-LICENSE").write_bytes(b"tampered license")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data, "license verification changed installed files"
        assert (installed / "rayline-core-LICENSE").read_bytes() == license_data
        (assets / "rayline-core-LICENSE").write_bytes(license_data)
        version = subprocess.check_output([str(installed / "rayline"), "--version"], text=True).split()[1]
        check = [str(installed / "rayline"), "update", "--check", "--version", version, "--install-path", str(installed / "rayline")]
        check_env = dict(os.environ, HOME=str(root))
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
        write_sums("\n".join(line for line in sums.splitlines() if "rayline-core" not in line) + "\n")
        install(False)
        for name, data in expected.items():
            assert (installed / name).read_bytes() == data
    print("Release installation passed: Raydock plus alias and core, required minisign and signature, "
          "--base-url/--version refusal, missing-core repair detection, tamper and missing-checksum refusal.")


if __name__ == "__main__":
    main()
