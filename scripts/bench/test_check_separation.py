"""Tests for check-separation.py's two rules."""
import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("check_separation", Path(__file__).with_name("check-separation.py"))
check_separation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_separation)


class SeparationTest(unittest.TestCase):
    def tree(self, files: dict[str, str]) -> Path:
        root = Path(tempfile.mkdtemp())
        for name, text in files.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text)
        return root

    def test_an_engine_depending_on_the_bench_fails_and_the_bench_itself_does_not(self):
        root = self.tree({"engine/Cargo.toml": '[package]\nname = "engine"\n[dev-dependencies]\nweft-bench = { path = "../weft-bench" }\n', "weft-bench/Cargo.toml": '[package]\nname = "weft-bench"\n[dependencies]\nweftdb = "1"\n'})
        self.assertEqual(check_separation.check(root), ["engine/Cargo.toml: crate engine depends on weft-bench"])

    def test_a_renamed_or_target_specific_dependency_is_still_found(self):
        manifest = {"package": {"name": "e"}, "target": {"cfg(unix)": {"dependencies": {"wb": {"package": "weft-xbench"}}}}}
        self.assertEqual(check_separation.manifest_problems(Path("e/Cargo.toml"), manifest), ["e/Cargo.toml: crate e depends on weft-xbench"])

    def test_engine_code_that_detects_a_benchmark_fails(self):
        problems = check_separation.source_problems(Path("src/lib.rs"), 'let fast = std::env::var("WEFT_BENCH_MODE").is_ok();\nuse weft_bench::Profile;\nlet doc = "read by weft-xbench";\n')
        self.assertEqual(len(problems), 2, problems)

    def test_comments_and_benches_are_not_engine_code(self):
        self.assertEqual(check_separation.source_problems(Path("src/lib.rs"), "// measured by weft-bench and weft_bench::run\nlet url = \"http://x\"; // see weft-xbench\n"), [])
        root = self.tree({"engine/benches/b.rs": 'std::env::var("BENCH_X");'})
        self.assertEqual(check_separation.check(root), [])


if __name__ == "__main__":
    unittest.main()
