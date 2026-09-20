"""Offline installer checks: python3 -m unittest discover -s tests."""

import hashlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "install.sh"
REPOSITORY = "https://github.com/preacherxp/super-docker"


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="sd-installer-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.bin = self.root / "mock-bin"
        self.bin.mkdir()
        self.destination = self.root / "install with spaces"
        self.env = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.defpath}",
            "SUPER_DOCKER_INSTALL_DIR": str(self.destination),
            "FIXTURE_DIR": str(self.root),
            "TEST_OS": "Linux",
            "TEST_ARCH": "x86_64",
        }
        self.command("uname", """import os, sys
print(os.environ['TEST_OS' if sys.argv[1] == '-s' else 'TEST_ARCH'])
""")
        self.command("curl", f"""import os, pathlib, shutil, sys
args = sys.argv[1:]
url = next(arg for arg in args if arg.startswith('https://'))
root = pathlib.Path(os.environ['FIXTURE_DIR'])
with (root / 'requests').open('a') as log:
    log.write(url + '\\n')
if url.endswith('/releases/latest'):
    print('{REPOSITORY}/releases/tag/v9.8.7', end='')
else:
    source = root / url.rsplit('/', 1)[1]
    if not source.exists():
        sys.exit(22)
    shutil.copyfile(source, args[args.index('-o') + 1])
""")
        for tool in ["cargo", "rustc", "git"]:
            self.command(tool, "raise RuntimeError('installer must not require a toolchain')")

    def command(self, name, source):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n{source}\n")
        path.chmod(0o755)

    def archive(self, target, *, symlink=False):
        path = self.root / f"super-docker-{target}.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            member = tarfile.TarInfo("sd")
            if symlink:
                member.type = tarfile.SYMTYPE
                member.linkname = "/bin/sh"
                archive.addfile(member)
            else:
                binary = b"#!/bin/sh\nprintf 'super-docker 9.8.7\\n'\n"
                member.size = len(binary)
                archive.addfile(member, io.BytesIO(binary))
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        path.with_suffix(".gz.sha256").write_text(f"{digest}  {path.name}\n")
        return path

    def install(self, *arguments):
        return subprocess.run(
            ["sh", "-s", "--", *arguments],
            input=SCRIPT.read_text(), env=self.env,
            text=True, capture_output=True, timeout=10,
        )

    def test_all_platforms_install_and_replace_both_aliases_without_rust(self):
        for system, machine, target in [
            ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
            ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
        ]:
            with self.subTest(target=target):
                self.archive(target)
                self.env.update(TEST_OS=system, TEST_ARCH=machine)
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)
                for name in ["sd", "super-docker"]:
                    version = subprocess.check_output([self.destination / name, "--version"])
                    self.assertEqual(version, b"super-docker 9.8.7\n")
                self.assertIn(str(self.destination), result.stdout)
                self.assertFalse(list(self.destination.glob(".super-docker.*")))

    def test_bad_checksum_leaves_existing_installation_untouched(self):
        archive = self.archive("x86_64-unknown-linux-musl")
        archive.write_bytes(archive.read_bytes() + b"corruption")
        self.destination.mkdir()
        for name in ["sd", "super-docker"]:
            (self.destination / name).write_text("old binary")
        result = self.install("v9.8.7")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum mismatch", result.stderr)
        for name in ["sd", "super-docker"]:
            self.assertEqual((self.destination / name).read_text(), "old binary")
        self.assertNotIn("/releases/latest", (self.root / "requests").read_text())

    def test_missing_release_and_symlink_archive_fail_before_installing(self):
        result = self.install("v9.8.7")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Download failed", result.stderr)
        self.archive("x86_64-unknown-linux-musl", symlink=True)
        result = self.install("v9.8.7")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("regular sd binary", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_invalid_tag_and_unsupported_platform_do_not_download(self):
        self.assertNotEqual(self.install("v1.2.3;id").returncode, 0)
        self.env["TEST_OS"] = "Windows"
        self.assertNotEqual(self.install().returncode, 0)
        self.env.update(TEST_OS="Linux", TEST_ARCH="riscv64")
        self.assertNotEqual(self.install().returncode, 0)
        self.assertFalse((self.root / "requests").exists())


if __name__ == "__main__":
    unittest.main()
