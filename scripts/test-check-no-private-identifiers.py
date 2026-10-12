#!/usr/bin/env python3
"""Exercise the checker in disposable repos, without real private identifiers."""
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
LOCK = (ROOT / "Cargo.lock").read_text()
ENTRIES = [
    next(block for block in LOCK.split("\n\n")
         if f'name = "{name}"\nversion = "{version}"' in block)
    for name, version in [("bip39", "2.2.2"), ("cbc", "0.1.2")]
]


class CheckNoPrivateIdentifiers(unittest.TestCase):
    def check(self, files, expected, private=None):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "scripts").mkdir()
            shutil.copyfile(ROOT / "scripts/check-no-private-identifiers.sh",
                            root / "scripts/check-no-private-identifiers.sh")
            subprocess.run(["git", "init", "-q", temp], check=True)
            for name, text in files.items():
                (root / name).write_text(text)
            if private is not None:
                (root / ".private-identifiers").write_text(private + "\n")
            subprocess.run(["git", "add", "--", *files], cwd=root, check=True)
            result = subprocess.run(["bash", "scripts/check-no-private-identifiers.sh"],
                                    cwd=root, capture_output=True, text=True)
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
            if expected:
                self.assertIn("[error]", result.stdout)

    def test_exact_public_registry_occurrences(self):
        for entry in ENTRIES:
            with self.subTest(entry=entry.splitlines()[1]):
                self.check({"Cargo.lock": entry + "\n"}, 0)

    def test_same_digits_outside_exact_occurrence(self):
        for entry in ENTRIES:
            digits = re.search(r"[0-9]{17,20}", entry).group()
            for path in ["Cargo.lock", "note.txt"]:
                with self.subTest(package=entry.splitlines()[1], path=path):
                    files = {"Cargo.lock": entry + "\n"}
                    files[path] = files.get(path, "") + f"# identifier = {digits}\n"
                    self.check(files, 1)
            self.check({"other.lock": entry + "\n"}, 1)

    def test_wrong_package_version_source_or_checksum(self):
        for entry in ENTRIES:
            checksum = re.search(r'checksum = "([a-f0-9]+)"', entry).group(1)
            variants = [
                re.sub(r'name = "[^"]+"', 'name = "other"', entry),
                re.sub(r'version = "[^"]+"', 'version = "0.0.0"', entry),
                entry.replace("crates.io-index", "other-index"),
                entry.replace(checksum, checksum[:-1] + "d"),
                entry.replace(f'checksum = "{checksum}"', f'checksum = "{checksum}" # extra'),
            ]
            for variant in variants:
                with self.subTest(variant=variant):
                    self.check({"Cargo.lock": variant + "\n"}, 1)

    def test_private_identifier_checks_still_apply(self):
        for entry in ENTRIES:
            digits = re.search(r"[0-9]{17,20}", entry).group()
            self.check({"Cargo.lock": entry + "\n"}, 1, private=digits)
        # A synthetic, non-service token also remains blocked by the private list.
        self.check({"note.txt": "fixture-private-token"}, 1, private="fixture-private-token")


if __name__ == "__main__":
    unittest.main()
