#!/usr/bin/env python3
"""Check acceptance evidence validation without running a harness or service."""
import copy
import runpy
from pathlib import Path
import unittest

CHECKS = runpy.run_path(str(Path(__file__).with_name("test-hybrid-live.py")))


class RoutingEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.decisions = {"result": "custom", "price_sha256": "a" * 64,
                          "routing": {"value_usd": .01, "price_profile_id": "local-discount", "price_profile_version": "v1"}}
        self.event = {"applied_value_usd": .01, "price_basis": "caller", "price_sha256": "a" * 64,
                      "price_profile": {"id": "local-discount", "version": "v1", "content_hash": "a" * 64}}

    def test_profile_and_inline_prices_are_admitted(self):
        for profile in (True, False):
            with self.subTest(profile=profile):
                decisions = copy.deepcopy(self.decisions)
                if not profile:
                    decisions["routing"] = {"value_usd": .01, "prices": {"local": {"input": 0}}}
                CHECKS["require_routing_config"](decisions)
                CHECKS["require_applied_routing"](decisions, [self.event])

    def test_unpinned_or_ambiguous_profile_refused(self):
        for missing in ("price_profile_id", "price_profile_version"):
            with self.subTest(missing=missing):
                decisions = copy.deepcopy(self.decisions)
                decisions["routing"].pop(missing)
                with self.assertRaises(AssertionError):
                    CHECKS["require_routing_config"](decisions)
        for change in ({"prices": {}}, {"price_profile_id": ""}):
            decisions = copy.deepcopy(self.decisions)
            decisions["routing"].update(change)
            with self.assertRaises(AssertionError):
                CHECKS["require_routing_config"](decisions)
        self.decisions.pop("price_sha256")
        with self.assertRaises(AssertionError):
            CHECKS["require_routing_config"](self.decisions)

    def test_every_decision_must_have_exact_applied_identity(self):
        changes = [{"applied_value_usd": .02}, {"price_basis": "catalog"}, {"price_sha256": "b" * 64},
                   {"price_profile": None}]
        for field in ("id", "version", "content_hash"):
            changed = dict(self.event["price_profile"], **{field: "unexpected"})
            changes.append({"price_profile": changed})
        for change in changes:
            with self.subTest(change=change):
                event = dict(self.event, **change)
                with self.assertRaises(AssertionError):
                    CHECKS["require_applied_routing"](self.decisions, [self.event, event])
        with self.assertRaises(AssertionError):
            CHECKS["require_applied_routing"](self.decisions, [])

    def test_refused_decision_stops_acceptance(self):
        with self.assertRaisesRegex(RuntimeError, "ARC decision refused"):
            CHECKS["require_admitted_decisions"]([{"msg": "decision refused", "reason": "unbound_action", "route_id": "test"}])


if __name__ == "__main__":
    unittest.main()
