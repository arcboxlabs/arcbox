"""Verify retained-network reuse rejects foreign resources and preserves aliases."""

import copy
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import run_clickhouse_restart as driver


NETWORK = {
    "Id": "a" * 64,
    "Name": "stall-warm-startup-contention-12345678",
    "Driver": "bridge",
    "Scope": "local",
    "Labels": {"arcbox.stall.experiment": "stall-warm-startup-contention-12345678"},
    "Containers": {},
    "IPAM": {"Config": [{"Subnet": "10.231.240.0/20"}]},
}


class NetworkReuseTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def test_rejects_foreign_active_and_nonlocal_networks(self):
        variants = [
            {"Labels": None},
            {"Labels": {"arcbox.stall.experiment": "another-experiment"}},
            {"Name": "user-network", "Labels": {"arcbox.stall.experiment": "user-network"}},
            {"Containers": {"other-container": {"Name": "active"}}},
            {"Driver": "overlay"},
            {"Scope": "swarm"},
        ]
        for changes in variants:
            with self.subTest(changes=changes), patch.object(driver, "api", return_value=NETWORK | changes):
                with self.assertRaises(RuntimeError):
                    driver.inspect_reusable_network(self.root, NETWORK["Id"], "inspect")

    def test_rejects_network_name_short_id_and_replacement(self):
        for identifier in (NETWORK["Name"], NETWORK["Id"][:12], "b" * 64):
            with self.subTest(identifier=identifier), patch.object(driver, "api", return_value=NETWORK):
                with self.assertRaisesRegex(RuntimeError, "exact network ID"):
                    driver.inspect_reusable_network(self.root, identifier, "inspect")

    def run_until_failure(self, identifier, inspections, message, label="case"):
        output = self.root / label
        args = SimpleNamespace(
            output=output,
            case="control",
            network=identifier,
            kernel_trace=None,
            futex_pc=None,
            native_observer=None,
            observer_policy="other",
        )
        seeds = {kind: (kind, kind + "-id", kind + "-image", kind + "-data", "/data") for kind in ("ch", "keeper")}
        records = {
            name: {
                "Id": identifier,
                "Image": image,
                "State": {"Running": False, "StartedAt": "retained-start"},
                "Config": {"Labels": {"com.docker.compose.project": "seeds"}},
                "HostConfig": {},
                "Mounts": [{"Name": volume, "Destination": destination}],
            }
            for name, identifier, image, volume, destination in seeds.values()
        }
        remaining = copy.deepcopy(inspections)

        def api(method, route, body=None):
            if method == "GET" and route.startswith("/networks/"):
                return remaining.pop(0)
            if method == "GET" and route.startswith("/images/"):
                return {}
            if method == "GET" and route == "/containers/udf/json":
                return {"State": {"Running": False}}
            if method == "GET" and route.startswith("/containers/"):
                return records[route.split("/")[2]]
            if method == "POST" and route == "/networks/create":
                return {"Id": "new-network-id"}
            if method == "POST" and route.startswith("/containers/create?"):
                raise RuntimeError("stop before container creation")
            self.fail(f"Unexpected API operation: {method} {route}")

        fixture = {"seed_project": "seeds", "udf_volume": {"writer": "udf"}}
        with patch.multiple(
            driver,
            create=True,
            SOCKET="/unused.sock",
            FIXTURE=fixture,
            SEEDS=seeds,
            CONFIGS=[],
            CONFIG_ROOT=self.root,
            UDF_VOLUME="udf-data",
        ):
            with patch.object(driver, "api", side_effect=api) as calls, patch.object(driver, "clone_volume") as copies:
                with self.assertRaisesRegex(RuntimeError, message):
                    driver.run(args)
        return output, calls, copies

    def test_rejects_foreign_network_before_copying_seeds(self):
        _, calls, copies = self.run_until_failure(
            NETWORK["Id"], [NETWORK | {"Labels": {}}], "retained restart experiment"
        )
        copies.assert_not_called()
        self.assertEqual([call.args[0] for call in calls.call_args_list], ["GET"])

    def test_rechecks_active_or_replaced_network_before_container_creation(self):
        variants = [
            ({"Containers": {"other": {}}}, "no active endpoints"),
            ({"Id": "b" * 64}, "exact network ID"),
        ]
        for number, (changes, message) in enumerate(variants):
            with self.subTest(changes=changes):
                output, calls, copies = self.run_until_failure(
                    NETWORK["Id"], [NETWORK, NETWORK | changes], message, f"case-{number}"
                )
                self.assertEqual(copies.call_count, 2)
                self.assertFalse((output / "ch-create.json").exists())
                self.assertTrue(all(call.args[0] == "GET" for call in calls.call_args_list))

    def test_uses_canonical_name_and_original_alias_without_creating_network(self):
        output, calls, _ = self.run_until_failure(NETWORK["Id"], [NETWORK, NETWORK], "stop before container creation")
        resources = json.loads((output / "resources.json").read_text())
        payload = json.loads((output / "ch-create.json").read_text())
        self.assertEqual(resources["network"], NETWORK["Name"])
        self.assertEqual(resources["reused_network_id"], NETWORK["Id"])
        self.assertEqual(payload["HostConfig"]["NetworkMode"], NETWORK["Name"])
        self.assertEqual(payload["NetworkingConfig"]["EndpointsConfig"], {NETWORK["Name"]: {"Aliases": ["ch"]}})
        self.assertFalse(any(call.args[:2] == ("POST", "/networks/create") for call in calls.call_args_list))
        self.assertEqual(json.loads((output / "network-initial.json").read_text()), NETWORK)
        self.assertEqual(json.loads((output / "network-before-create.json").read_text()), NETWORK)
        cleanup = json.loads((output / "events.jsonl").read_text().splitlines()[-1])
        self.assertEqual(cleanup["retained_network"], NETWORK["Name"])

    def test_default_still_creates_a_new_owned_network(self):
        output, calls, _ = self.run_until_failure(None, [], "stop before container creation")
        resources = json.loads((output / "resources.json").read_text())
        creations = [call for call in calls.call_args_list if call.args[:2] == ("POST", "/networks/create")]
        self.assertEqual(len(creations), 1)
        self.assertEqual(
            creations[0].args[2],
            {"Name": resources["prefix"], "Labels": {"arcbox.stall.experiment": resources["prefix"]}},
        )
        self.assertEqual(resources["network"], resources["prefix"])
        self.assertNotIn("reused_network_id", resources)


if __name__ == "__main__":
    unittest.main(verbosity=2)
