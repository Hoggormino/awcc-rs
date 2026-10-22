#!/usr/bin/env python3
"""Run the rendered native maintainer entrypoints against isolated filesystem/service fixtures.

No root, host service changes, or hardware are needed. The production scripts have fixed paths;
only these test copies substitute the filesystem root and expected owner. External service tools
are controlled substitutes, while authorization/marker/config preservation use real files.
"""

import fcntl
import configparser
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parent.parent
RENDER = REPO / "scripts/render-native-package-hook.sh"

MOCK_TOOL = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
root = pathlib.Path(os.environ["AWCC_NATIVE_FIXTURE"])
state_path = root / "service.json"
state = json.loads(state_path.read_text())
tool = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as stream:
    stream.write(json.dumps([tool] + args) + "\n")
code = 0
if tool == "awcc-cli":
    assert args[:2] == ["service", "authorize"], args
    if state.get("authorize_fails"):
        code = 1
    else:
        target = root / "etc/awcc/authorized-users"
        target.write_text(args[2] + "\n")
        target.chmod(0o600)
elif tool in ("systemctl", "deb-systemd-helper", "deb-systemd-invoke"):
    action = args[0]
    denied = tool == "deb-systemd-invoke" and state.get("policy_denied")
    if action == "is-active":
        code = 0 if state["active"] else 3
    elif action == "show":
        if state.get("show_fails"):
            code = 1
        else:
            print(state.get("fragment", str(root / "usr/lib/systemd/system/awccd.service")))
    elif action == "enable":
        state["enabled"] = True
    elif action in ("disable", "purge"):
        if action == "purge" and state.get("helper_purge_fails"):
            code = 1
        else:
            state["enabled"] = False
    elif action == "start" and not denied:
        if state.get("start_fails"):
            code = 1
        else:
            state["active"] = True
    elif action == "try-restart" and not denied:
        if state["active"]:
            state["restarts"] += 1
    elif action == "stop" and not denied:
        if state.get("stop_fails"):
            code = 1
        else:
            state["active"] = False
            shutil.rmtree(root / "run/awcc", ignore_errors=True)
elif tool == "modprobe":
    if state.get("module_fails"):
        code = 1
    else:
        target = root / "proc/acpi/call"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("")
elif tool == "policy-rc.d":
    code = 101 if state.get("policy_denied") else 0
elif tool != "udevadm":
    raise AssertionError(tool)
state_path.write_text(json.dumps(state))
sys.exit(code)
'''


class PackageLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="awcc-native-hooks-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "root with spaces"
        for path in (
            "etc/awcc", "etc/systemd/system", "var/lib", "usr/bin", "usr/sbin",
            "usr/lib/systemd/system", "usr/local/lib/systemd/system", "run/systemd/system",
            "run/udev", "run/awcc", "mock-bin",
        ):
            (self.root / path).mkdir(parents=True, exist_ok=True)
        (self.root / "etc/passwd").write_text(
            "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000:Alice:/home/alice:/bin/sh\n"
            "bob:x:1001:1001:Bob:/home/bob:/bin/sh\n"
        )
        self.vendor = self.root / "usr/lib/systemd/system/awccd.service"
        self.vendor.write_text("[Service]\nExecStart=/usr/bin/awcc-cli -d\n")
        self.auth = self.root / "etc/awcc/authorized-users"
        self.settings = self.root / "var/lib/awcc/settings.json"
        self.marker = self.root / "var/lib/awcc/native-package-initialized"
        self.save_state(enabled=False, active=False, restarts=0)
        tool = self.root / "mock-bin/tool"
        tool.write_text(MOCK_TOOL)
        tool.chmod(0o755)
        for name in ("systemctl", "deb-systemd-helper", "deb-systemd-invoke", "modprobe", "udevadm"):
            (self.root / f"mock-bin/{name}").symlink_to(tool)
        (self.root / "usr/bin/awcc-cli").symlink_to(tool)
        (self.root / "usr/sbin/policy-rc.d").symlink_to(tool)
        self.env = dict(os.environ)
        for name in ("PKEXEC_UID", "SUDO_UID", "SUDO_USER", "USER", "LOGNAME"):
            self.env.pop(name, None)
        self.env.update(
            PATH=str(self.root / "mock-bin") + os.pathsep + os.environ["PATH"],
            AWCC_NATIVE_FIXTURE=str(self.root),
            SUDO_UID="1000",
        )

    def state(self):
        return json.loads((self.root / "service.json").read_text())

    def save_state(self, **values):
        (self.root / "service.json").write_text(json.dumps(values))

    def update_state(self, **values):
        current = self.state()
        current.update(values)
        self.save_state(**current)

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def clear_calls(self):
        (self.root / "calls.jsonl").unlink(missing_ok=True)

    def run_hook(self, hook, *args, function=None, expected=0):
        script = subprocess.check_output(["bash", str(RENDER), hook], text=True)
        script = script.replace("AWCC_NATIVE_ROOT=''", "AWCC_NATIVE_ROOT=" + shlex.quote(str(self.root)), 1)
        script = script.replace("AWCC_NATIVE_OWNER=0", f"AWCC_NATIVE_OWNER={os.getuid()}", 1)
        if function:
            script += "\n" + function + ' "$@"\n'
        path = Path(self.tmp.name) / "rendered-hook"
        path.write_text(script)
        result = subprocess.run(["sh", str(path), *args], env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        return result

    def configure(self, fmt, fresh=True):
        if fmt == "arch":
            return self.run_hook("arch.install", "0.1.3", function="post_install" if fresh else "post_upgrade")
        if fmt == "rpm":
            return self.run_hook("rpm.post", "1" if fresh else "2")
        return self.run_hook("deb.postinst", "configure", *([] if fresh else ["0.1.2"]))

    def remove(self, fmt, expected=0):
        if fmt == "arch":
            return self.run_hook("arch.pre-remove", expected=expected)
        if fmt == "rpm":
            return self.run_hook("rpm.preun", "0", expected=expected)
        return self.run_hook("deb.prerm", "remove", expected=expected)

    def test_fresh_install_authorizes_enables_and_starts_each_format(self):
        for fmt in ("arch", "deb", "rpm"):
            with self.subTest(fmt=fmt):
                self.auth.unlink(missing_ok=True)
                self.marker.unlink(missing_ok=True)
                self.update_state(enabled=False, active=False)
                self.configure(fmt)
                self.assertEqual(self.auth.read_text(), "1000\n")
                self.assertEqual(self.marker.stat().st_mode & 0o777, 0o600)
                self.assertTrue(self.state()["enabled"])
                self.assertTrue(self.state()["active"])

    def test_pkexec_identity_takes_precedence(self):
        self.env["PKEXEC_UID"] = "1001"
        self.configure("arch")
        self.assertEqual(self.auth.read_text(), "1001\n")

    def test_unidentified_or_invalid_invoker_never_guesses_an_account(self):
        self.env.update(USER="alice", LOGNAME="alice", SUDO_USER="alice")
        for value in (None, "", "0", "4294967295", "9999", "1000\n1001", "+1000", "-1", "9" * 80):
            with self.subTest(value=value):
                self.marker.unlink(missing_ok=True)
                if value is None:
                    self.env.pop("SUDO_UID", None)
                else:
                    self.env["SUDO_UID"] = value
                result = self.configure("arch")
                self.assertFalse(self.auth.exists())
                self.assertIn("Set up service", result.stderr)
        self.env.update(PKEXEC_UID="invalid", SUDO_UID="1000")
        self.marker.unlink()
        self.configure("arch")
        self.assertFalse(self.auth.exists())

    def test_existing_empty_or_populated_policy_is_not_modified(self):
        for contents in ("", "# administrator policy\n1001\n"):
            with self.subTest(contents=contents):
                self.marker.unlink(missing_ok=True)
                self.auth.write_text(contents)
                self.configure("rpm")
                self.assertEqual(self.auth.read_text(), contents)

    def test_upgrade_preserves_disabled_stopped_and_revoked_state(self):
        for fmt in ("arch", "deb", "rpm"):
            with self.subTest(fmt=fmt):
                self.configure(fmt)
                self.auth.unlink(missing_ok=True)
                self.update_state(enabled=False, active=False)
                self.clear_calls()
                self.configure(fmt, fresh=False)
                self.assertFalse(self.auth.exists())
                self.assertFalse(self.state()["enabled"])
                self.assertFalse(self.state()["active"])
                self.assertFalse(any(call[0] == "awcc-cli" for call in self.calls()))
                self.assertFalse(any(call[1] in ("enable", "start") for call in self.calls()))

    def test_upgrade_restarts_active_service_without_enabling_it(self):
        self.update_state(enabled=False, active=True)
        for fmt in ("arch", "deb", "rpm"):
            with self.subTest(fmt=fmt):
                self.configure(fmt, fresh=False)
                self.assertFalse(self.state()["enabled"])
                self.assertTrue(self.state()["active"])
        self.assertEqual(self.state()["restarts"], 3)

    def test_reinstall_does_not_restore_removed_policy(self):
        self.configure("arch")
        self.auth.unlink()
        self.configure("arch")
        self.assertFalse(self.auth.exists())

    def test_mask_or_foreign_unit_prevents_authorization_and_service_changes(self):
        override = self.root / "etc/systemd/system/awccd.service"
        override.symlink_to("/dev/null")
        for fmt in ("arch", "deb", "rpm"):
            self.configure(fmt)
            self.remove(fmt)
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.marker.exists())
        self.assertFalse(any(call[0] in ("systemctl", "deb-systemd-helper", "awcc-cli")
                             and call[1] != "is-active" for call in self.calls()))
        self.assertTrue(override.is_symlink())

    def test_masked_running_service_blocks_removal_and_preserves_mask(self):
        override = self.root / "etc/systemd/system/awccd.service"
        override.symlink_to("/dev/null")
        self.update_state(enabled=True, active=True)
        for fmt in ("arch", "deb", "rpm"):
            result = self.remove(fmt, expected=1)
            self.assertIn("masked service is still active", result.stderr)
        self.assertTrue(self.state()["active"])
        self.assertTrue(override.is_symlink())

    def test_symlinked_configuration_directory_is_not_followed(self):
        foreign = self.root / "foreign"
        foreign.mkdir()
        (self.root / "etc/awcc").rmdir()
        (self.root / "etc/awcc").symlink_to(foreign, target_is_directory=True)
        self.configure("arch")
        self.assertEqual(list(foreign.iterdir()), [])
        self.assertFalse(self.state()["enabled"])
        self.assertFalse(self.state()["active"])

    def test_writable_state_ancestor_prevents_automatic_setup(self):
        (self.root / "var/lib").chmod(0o777)
        self.configure("arch")
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.state()["enabled"])

    def test_transient_and_differently_selected_units_are_not_managed(self):
        generated = self.root / "run/systemd/transient/awccd.service"
        generated.parent.mkdir()
        generated.write_text("foreign unit")
        self.configure("arch")
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.state()["active"])
        generated.unlink()
        self.update_state(fragment=str(self.root / "foreign.service"))
        self.configure("arch")
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.state()["active"])
        self.remove("arch", expected=1)

    def test_systemd_query_failure_stops_setup_before_authorization(self):
        self.update_state(show_fails=True)
        self.configure("arch")
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.state()["enabled"])

    def test_parallel_installer_blocks_package_mutations(self):
        state_dir = self.root / "var/lib/awcc"
        state_dir.mkdir()
        with (state_dir / ".install.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.configure("arch")
            self.assertFalse(self.auth.exists())
            self.assertFalse(self.state()["active"])
            self.remove("arch", expected=1)

    def test_offline_install_configures_next_boot_without_touching_kernel(self):
        shutil.rmtree(self.root / "run/systemd")
        self.configure("deb")
        self.assertTrue(self.state()["enabled"])
        self.assertFalse(self.state()["active"])
        self.assertEqual(self.auth.read_text(), "1000\n")
        self.assertFalse(any(call[0] in ("modprobe", "deb-systemd-invoke", "systemctl") for call in self.calls()))

    def test_debian_policy_blocks_start_and_module_loading(self):
        self.update_state(policy_denied=True)
        self.configure("deb")
        self.assertTrue(self.state()["enabled"])
        self.assertFalse(self.state()["active"])
        self.assertFalse(any(call[0] == "modprobe" for call in self.calls()))

    def test_missing_module_does_not_leave_package_unconfigured(self):
        self.update_state(module_fails=True, start_fails=True)
        result = self.configure("rpm")
        self.assertIn("acpi_call is unavailable", result.stderr)
        self.assertIn("Set up service", result.stderr)
        self.assertTrue(self.marker.exists())
        self.assertEqual(self.auth.read_text(), "1000\n")

    def test_removal_stops_disables_and_preserves_user_configuration(self):
        for fmt in ("arch", "deb", "rpm"):
            with self.subTest(fmt=fmt):
                self.configure(fmt)
                self.settings.write_text('{"user": "settings"}\n')
                (self.root / "run/awcc").mkdir(exist_ok=True)
                (self.root / "run/awcc/socket").write_text("active")
                self.remove(fmt)
                self.assertFalse(self.state()["active"])
                self.assertFalse(self.state()["enabled"])
                self.assertFalse((self.root / "run/awcc").exists())
                self.assertEqual(self.settings.read_text(), '{"user": "settings"}\n')
                self.assertTrue(self.auth.exists())

    def test_failed_stop_aborts_removal_before_disabling_service(self):
        self.update_state(enabled=True, active=True, stop_fails=True)
        for fmt in ("arch", "deb", "rpm"):
            self.remove(fmt, expected=1)
            self.assertTrue(self.state()["enabled"])
            self.assertTrue(self.state()["active"])

    def test_arch_removal_uses_abortable_transaction_hook_only_for_removal(self):
        hook = configparser.ConfigParser(allow_no_value=True)
        hook.read(REPO / "packaging/linux/05-awcc-rs-remove.hook")
        self.assertEqual(dict(hook["Trigger"]), {
            "operation": "Remove", "type": "Package", "target": "awcc-rs",
        })
        self.assertEqual(hook["Action"]["When"], "PreTransaction")
        self.assertIn("AbortOnFail", hook["Action"])
        self.assertEqual(hook["Action"]["Exec"], "/usr/share/libalpm/scripts/awcc-rs-pre-remove")
        self.update_state(enabled=True, active=True, stop_fails=True)
        self.remove("arch", expected=1)
        self.assertTrue(self.state()["active"])
        self.update_state(stop_fails=False)
        self.remove("arch")
        self.vendor.unlink()
        self.run_hook("arch.install", "0.1.3", function="post_remove")
        self.assertIn(["systemctl", "daemon-reload"], self.calls())

    def test_old_package_upgrade_erasure_never_stops_or_disables(self):
        self.update_state(enabled=True, active=True)
        self.run_hook("rpm.preun", "1")
        self.run_hook("rpm.postun", "1")
        self.run_hook("deb.prerm", "upgrade", "0.1.4")
        self.run_hook("deb.postrm", "upgrade", "0.1.4")
        self.assertTrue(self.state()["enabled"])
        self.assertTrue(self.state()["active"])
        self.assertFalse(any(call[1] in ("stop", "disable", "reset-failed") for call in self.calls()))

    def test_debian_purge_works_without_binary_and_keeps_unknown_files(self):
        self.configure("deb")
        self.settings.write_text("settings")
        known_log = self.root / "var/lib/awcc/awcc-rs/logs/evidence-abc123.json"
        known_log.parent.mkdir(parents=True)
        known_log.write_text("evidence")
        temp_auth = self.auth.parent / ".authorized-users.123.tmp"
        temp_auth.write_text("incomplete")
        unknown = self.root / "var/lib/awcc/admin-note.txt"
        unknown.write_text("preserve")
        database = self.root / "etc/awcc/database.json"
        database.write_text("admin database")
        self.remove("deb")
        self.vendor.unlink()
        (self.root / "usr/bin/awcc-cli").unlink()
        self.run_hook("deb.postrm", "purge")
        self.assertFalse(self.auth.exists())
        self.assertFalse(self.settings.exists())
        self.assertFalse(self.marker.exists())
        self.assertFalse(known_log.exists())
        self.assertFalse(temp_auth.exists())
        self.assertTrue((self.root / "var/lib/awcc/.install.lock").is_file())
        self.assertEqual(unknown.read_text(), "preserve")
        self.assertEqual(database.read_text(), "admin database")

    def test_purge_refuses_symlinked_data_and_foreign_installer_ownership(self):
        self.configure("deb")
        self.vendor.unlink()
        foreign = self.root / "precious.json"
        foreign.write_text("precious")
        self.settings.symlink_to(foreign)
        self.run_hook("deb.postrm", "purge")
        self.assertTrue(self.settings.is_symlink())
        self.assertEqual(foreign.read_text(), "precious")
        self.auth.parent.mkdir(parents=True, exist_ok=True)
        self.auth.write_text("1001\n")
        (self.root / "var/lib/awcc/install-owner.json").write_text("AppImage ownership")
        self.run_hook("deb.postrm", "purge")
        self.assertEqual(self.auth.read_text(), "1001\n")

    def test_purge_io_and_helper_failures_remain_retryable(self):
        self.configure("deb")
        self.vendor.unlink()
        failing_rm = self.root / "mock-bin/rm"
        failing_rm.write_text("#!/bin/sh\nexit 1\n")
        failing_rm.chmod(0o755)
        self.run_hook("deb.postrm", "purge", expected=1)
        self.assertTrue(self.auth.exists())
        self.assertTrue(self.marker.exists())
        failing_rm.unlink()
        self.update_state(helper_purge_fails=True)
        self.run_hook("deb.postrm", "purge", expected=1)
        self.assertFalse(self.auth.exists())
        self.update_state(helper_purge_fails=False)
        self.run_hook("deb.postrm", "purge")

    def test_udev_triggers_only_supported_controllers(self):
        self.configure("arch")
        triggers = [call for call in self.calls() if call[:2] == ["udevadm", "trigger"]]
        self.assertEqual(len(triggers), 2)
        for product, call in zip(("0550", "0551"), triggers):
            self.assertIn("--subsystem-match=usb", call)
            self.assertIn("--attr-match=idVendor=187c", call)
            self.assertIn("--attr-match=idProduct=" + product, call)


if __name__ == "__main__":
    unittest.main(verbosity=2)
