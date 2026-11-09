import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location("check_rust_versions", pathlib.Path(__file__).with_name("check-rust-versions.py"))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def package(name, rust_version=None):
    return {"id": name, "name": name, "version": "1.0.0", "rust_version": rust_version}


def metadata(packages, edges, members):
    """edges maps a package to [(dependency, kind)], where kind is None for a normal dependency, "build" or "dev"."""
    return {
        "packages": packages,
        "workspace_members": members,
        "resolve": {"nodes": [{"id": p["id"], "deps": [{"pkg": dep, "dep_kinds": [{"kind": kind}]} for dep, kind in edges.get(p["id"], [])]} for p in packages]},
    }


class CheckRustVersions(unittest.TestCase):
    def test_declared_equal_to_the_dependency_floor_passes(self):
        data = metadata([package("lib", "1.90"), package("dep", "1.90")], {"lib": [("dep", None)]}, ["lib"])
        self.assertEqual(check.mismatches(data), [])

    def test_a_crate_declaring_more_than_its_tree_needs_is_reported(self):
        data = metadata([package("lib", "1.90"), package("dep", "1.88")], {"lib": [("dep", None)]}, ["lib"])
        self.assertEqual(len(check.mismatches(data)), 1)
        self.assertIn("require 1.88.0", check.mismatches(data)[0])

    def test_a_crate_declaring_less_than_its_tree_needs_is_reported(self):
        data = metadata([package("lib", "1.88"), package("dep", "1.90")], {"lib": [("dep", None)]}, ["lib"])
        self.assertEqual(len(check.mismatches(data)), 1)

    def test_a_crate_without_a_declaration_is_reported(self):
        data = metadata([package("lib"), package("dep", "1.88")], {"lib": [("dep", None)]}, ["lib"])
        self.assertEqual(len(check.mismatches(data)), 1)

    def test_a_dev_dependency_does_not_raise_the_floor(self):
        data = metadata([package("lib", "1.88"), package("dep", "1.88"), package("tester", "1.95")], {"lib": [("dep", None), ("tester", "dev")]}, ["lib"])
        self.assertEqual(check.mismatches(data), [])

    def test_a_build_dependency_raises_the_floor(self):
        data = metadata([package("lib", "1.88"), package("tool", "1.90")], {"lib": [("tool", "build")]}, ["lib"])
        self.assertEqual(len(check.mismatches(data)), 1)

    def test_a_transitive_dependency_raises_the_floor(self):
        data = metadata([package("lib", "1.90"), package("mid", "1.80"), package("leaf", "1.90")], {"lib": [("mid", None)], "mid": [("leaf", None)]}, ["lib"])
        self.assertEqual(check.mismatches(data), [])

    def test_the_language_floor_applies_when_no_dependency_declares_more(self):
        data = metadata([package("lib", "1.86"), package("dep", "1.70")], {"lib": [("dep", None)]}, ["lib"])
        self.assertEqual(check.mismatches(data), [])

    def test_a_workspace_members_own_value_does_not_set_another_floor(self):
        data = metadata([package("app", "1.88"), package("lib", "1.95"), package("dep", "1.88")], {"app": [("lib", None)], "lib": [("dep", None)]}, ["app", "lib"])
        self.assertEqual([m.split()[0] for m in check.mismatches(data)], ["lib"])


if __name__ == "__main__":
    unittest.main()
