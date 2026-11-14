#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""CI RootFS delegation checks; no downloads, images or VMs."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / 'scripts/ci/prepare_rootfs_cache.sh'


class RootfsDelegationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.helper = self.root / 'harness/scripts/ensure_rootfs_release_cache.sh'
        self.helper.parent.mkdir(parents=True)
        self.helper.write_text('''#!/bin/sh
set -eu
printf '%s|%s|%s|%s\\n' "$*" "$ROOTFS_CACHE_DIR" "$ROOTFS_VARIANT" "$ROOTFS_RELEASE_TAG" >> "$CALLS"
[ "${FAIL_DOWNLOAD:-0}" = 0 ] || exit 7
printf '%s/rootfs-%s-%s.img\\n' "$ROOTFS_CACHE_DIR" "$2" "$ROOTFS_VARIANT"
''')
        self.helper.chmod(0o755)
        checker = self.helper.parent / 'check-longevity-glibc-bins.sh'
        checker.write_text('#!/bin/sh\ntest "$ROOTFS_VARIANT" = xkernel-glibc\nexit "${FAIL_LINKAGE:-0}"\n')
        checker.chmod(0o755)
        self.calls = self.root / 'calls'
        self.env = {**os.environ, 'HARNESS_ROOTFS_DIR': str(self.helper.parents[1]),
                    'ROOTFS_CACHE_DIR': str(self.root / 'cache'), 'CALLS': str(self.calls),
                    'ROOTFS_VARIANT': 'xkernel-glibc', 'ROOTFS_RELEASE_TAG': 'rootfs-test'}

    def run_script(self, *arches, **env):
        return subprocess.run([str(SCRIPT), *arches], env={**self.env, **env},
                              text=True, capture_output=True, timeout=5)

    def test_same_helper_serves_all_arches_with_release_and_libc_identity(self):
        result = self.run_script('aarch64', 'x86_64', 'riscv64')
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = self.calls.read_text().splitlines()
        self.assertEqual(len(lines), 3)
        for arch, line in zip(('aarch64', 'x86_64', 'riscv64'), lines):
            self.assertIn(f'--arch {arch} --variant xkernel-glibc', line)
            self.assertTrue(line.endswith('|xkernel-glibc|rootfs-test'))
            self.assertIn(f'rootfs-{arch}-xkernel-glibc.img', result.stdout)

    def test_musl_override_is_rejected_before_download(self):
        result = self.run_script('x86_64', ROOTFS_VARIANT='xkernel-musl')
        self.assertEqual(result.returncode, 2)
        self.assertFalse(self.calls.exists())

    def test_download_failure_is_not_hidden(self):
        result = self.run_script('aarch64', FAIL_DOWNLOAD='1')
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, '')

    def test_wrong_libc_artifact_fails_before_publishing_path(self):
        result = self.run_script('aarch64', FAIL_LINKAGE='8')
        self.assertEqual(result.returncode, 8)
        self.assertEqual(result.stdout, '')

    def test_unknown_arch_is_rejected_before_download(self):
        self.assertEqual(self.run_script('../other').returncode, 2)
        self.assertFalse(self.calls.exists())

    def test_missing_harness_does_not_fall_back_to_old_image(self):
        result = self.run_script('aarch64', HARNESS_ROOTFS_DIR=str(self.root / 'absent'))
        self.assertEqual(result.returncode, 2)
        self.assertIn('HARNESS_ROOTFS_DIR', result.stderr)


if __name__ == '__main__':
    unittest.main()
