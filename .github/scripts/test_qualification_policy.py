"""Credential-free checks of qualification decisions and evidence boundaries."""

import json
import unittest
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
from qualification_policy import run_probe
from qualification_policy import EvidenceError, PROVIDERS, evaluate, parse_record, summary_providers

SHA = "a" * 40


def record(provider="mistral", outcome="pass", **changes):
    value = dict(schema_version=1, candidate=SHA, provider=provider, run_id="123", run_attempt="2",
                 probe_id=(sorted(PROVIDERS).index(provider) + 1).__format__("032x"), outcome=outcome)
    value.update(changes)
    return json.dumps(value)


def decision(provider, raw, **changes):
    arguments = dict(candidate=SHA, run_id="123", run_attempt="2")
    arguments.update(changes)
    return evaluate(provider, raw, **arguments)


class QualificationPolicyTests(unittest.TestCase):
    def test_primary_success_is_required_even_if_legacy_enrollment_is_off(self):
        for provider in ["openai", "anthropic"]:
            self.assertTrue(decision(provider, record(provider), enrollment="false")["accepted"])
            for result in ["failure", "rate_limited"]:
                self.assertFalse(decision(provider, record(provider, result))["accepted"])
            for job in ["failure", "skipped", "cancelled", "timed_out", ""]:
                with self.assertRaises(EvidenceError):
                    decision(provider, record(provider), job=job)

    def test_only_typed_supplemental_rate_limiting_is_nonblocking(self):
        for provider in ["gemini", "mistral", "xai"]:
            value = decision(provider, record(provider, "rate_limited"))
            self.assertEqual(value["status"], "unverified")
            self.assertTrue(value["accepted"])
            self.assertFalse(decision(provider, record(provider, "failure"))["accepted"])
        for outcome in ["timeout", "unknown", "unauthorized", "invalidresponse", "503", "429"]:
            with self.assertRaises(EvidenceError):
                decision("mistral", record(outcome=outcome))

    def test_unconfigured_is_never_a_pass_or_silently_executed(self):
        for enrollment in ["", "false"]:
            value = decision("mistral", "", job="skipped", enrollment=enrollment)
            self.assertEqual(value["status"], "not_configured")
            with self.assertRaises(EvidenceError):
                decision("mistral", record(), enrollment=enrollment)
        with self.assertRaises(EvidenceError):
            decision("mistral", "", job="skipped", enrollment="TRUE")

    def test_fail_closed_for_invalid_missing_stale_or_conflicting_evidence(self):
        for raw in ["", "{}", "null", "[]", "x" * 2049, record()[:-1] + ', "outcome":"pass"}']:
            with self.subTest(raw=raw[:30]), self.assertRaises(EvidenceError):
                decision("mistral", raw)
        for changes in [dict(candidate="b"*40), dict(provider="gemini"), dict(run_id="124"), dict(run_attempt="1"),
                        dict(probe_id="bad"), dict(schema_version=True), dict(extra="untrusted"), dict(outcome="pass\nforge=1")]:
            with self.subTest(changes=changes), self.assertRaises(EvidenceError):
                decision("mistral", record(**changes))
        with self.assertRaises(EvidenceError):
            decision("mistral", record(), probe_id="0"*32)
        with self.assertRaises(EvidenceError):
            decision("mistral", record("mistral", "rate_limited"), job="failure")

    def test_complete_aggregate_and_mixed_warning_plus_defect(self):
        records = {p: record(p) for p in PROVIDERS}
        jobs = dict.fromkeys(PROVIDERS, "success")
        enabled = dict.fromkeys(PROVIDERS, "true")
        records["mistral"] = record("mistral", "rate_limited")
        def aggregate():
            return summary_providers(records, jobs, enabled, candidate=SHA, run_id="123", run_attempt="2")
        self.assertTrue(all(v["accepted"] for v in aggregate().values()))
        records["xai"] = record("xai", "failure")
        self.assertFalse(all(v["accepted"] for v in aggregate().values()))
        records["xai"] = record("xai", probe_id=json.loads(records["mistral"])["probe_id"])
        with self.assertRaises(EvidenceError):
            aggregate()
        del records["xai"]
        with self.assertRaises(EvidenceError):
            aggregate()

    def test_evidence_has_only_allowlisted_single_line_fields(self):
        value = parse_record(record())
        self.assertFalse(any(word in value for word in ["token", "prompt", "message", "home", "response"]))

    def test_retry_reuses_only_the_matching_completed_producer_attempt(self):
        records = {p: record(p) for p in PROVIDERS}
        records["openai"] = record("openai", run_attempt="1")
        arguments = dict(candidate=SHA, run_id="123", run_attempt="2")
        jobs = dict.fromkeys(PROVIDERS, "success")
        enabled = dict.fromkeys(PROVIDERS, "true")
        attempts = dict.fromkeys(PROVIDERS, "2")
        with self.assertRaises(EvidenceError):
            summary_providers(records, jobs, enabled, **arguments, job_attempts=attempts)
        attempts["openai"] = "1"
        self.assertTrue(all(r["accepted"] for r in summary_providers(records, jobs, enabled, **arguments, job_attempts=attempts).values()))
        for value in ["0", "3", "invalid", True]:
            with self.subTest(value=value), self.assertRaises(EvidenceError):
                summary_providers(records, jobs, enabled, **arguments, job_attempts={**attempts, "openai": value})

    def test_probe_entrypoint_requires_valid_producer_completion_and_identity(self):
        for provider, outcome, code, alter, accepted in [
            ("mistral", "rate_limited", 0, {}, True),
            ("openai", "pass", 0, {}, True),
            ("anthropic", "rate_limited", 0, {}, False),
            ("mistral", "failure", 0, {}, False),
            ("mistral", "rate_limited", 1, {}, False),
            ("mistral", "rate_limited", 0, {"probe_id": "0"*32}, False),
            ("mistral", "rate_limited", 0, {"run_attempt": "1"}, False),
        ]:
            with self.subTest(provider=provider, outcome=outcome, code=code, alter=alter), tempfile.TemporaryDirectory() as temp:
                output = Path(temp)/"output"
                environment = dict(CARGO_AI_SHA=SHA, RUNNER_TEMP=temp, GITHUB_RUN_ID="123", GITHUB_RUN_ATTEMPT="2", GITHUB_OUTPUT=str(output))
                def produce(command, *, env, check):
                    self.assertIn(f"live_{provider}_smoke_uses_isolated_stdin_credentials", command)
                    self.assertEqual(command[-2:], ["--ignored", "--exact"])
                    Path(env["CARGO_AI_QUALIFICATION_REPORT"]).write_text(record(provider, outcome, **{"probe_id": env["CARGO_AI_QUALIFICATION_PROBE"], **alter}), encoding="utf-8")
                    return SimpleNamespace(returncode=code)
                with patch.dict(os.environ, environment), patch("qualification_policy.subprocess.check_output", return_value=SHA+"\n"), patch("qualification_policy.subprocess.run", side_effect=produce):
                    if accepted:
                        self.assertEqual(run_probe(provider), 0)
                        self.assertEqual(parse_record(output.read_text(encoding="utf-8").removeprefix("evidence=").strip())["outcome"], outcome)
                    else:
                        with self.assertRaises(EvidenceError):
                            run_probe(provider)
                        self.assertFalse(output.exists())


class QualificationDashboardTests(unittest.TestCase):
    def run_dashboard(self, change=None):
        import shutil
        import subprocess
        import sys
        import textwrap
        repository = Path(__file__).resolve().parents[2]
        workflow = (repository/".github/workflows/release-qualification.yml").read_text(encoding="utf-8")
        source = textwrap.dedent(workflow.split("          python3 - <<'PY'\n", 1)[1].rsplit("          PY", 1)[0])
        names = [f"{family} ({platform}-latest)" for family in ["Deterministic qualification", "Source package qualification"] for platform in ["ubuntu", "macos", "windows"]]
        labels = {"openai": "OpenAI", "anthropic": "Anthropic", "gemini": "Gemini", "xai": "xAI", "mistral": "Mistral"}
        names += [f"Live {label} conformance" for label in labels.values()]
        jobs = [dict(name=name, head_sha=SHA, run_attempt=2, status="completed", conclusion="success", completed_at="2026-01-01T00:00:00Z", html_url=f"https://github.com/example/project/actions/runs/123/job/{index}") for index, name in enumerate(names)]
        needs = {"live_"+p: {"result": "success", "outputs": {"evidence": record(p, "rate_limited" if p == "mistral" else "pass")}} for p in PROVIDERS}
        extra = {}
        if change:
            change(jobs, needs, extra)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root/"qualification-dashboard-source/.github"
            (checkout/"scripts").mkdir(parents=True)
            shutil.copy2(repository/".github/scripts/qualification_policy.py", checkout/"scripts/qualification_policy.py")
            shutil.copy2(repository/".github/package-qualification-catalog.toml", checkout/"package-qualification-catalog.toml")
            jobs_path = root/"jobs.json"; jobs_path.write_text(json.dumps({"jobs": jobs}), encoding="utf-8")
            summary = root/"summary.md"
            env = dict(os.environ, GITHUB_REPOSITORY="example/project", GITHUB_RUN_ID="123", GITHUB_RUN_NUMBER="1", GITHUB_RUN_ATTEMPT="2", CARGO_AI_SHA=SHA, TRUSTED_TRIGGER_SHA=SHA, JOBS_API_STATUS="0", JOBS_JSON=str(jobs_path), CATALOG_CHECKOUT_OUTCOME="success", CATALOG_PATH=str(checkout/"package-qualification-catalog.toml"), GITHUB_STEP_SUMMARY=str(summary), PROVIDER_PROBE_RECORDS=json.dumps(needs), DETERMINISTIC_RESULT="success", PACKAGE_RESULT="success", LIVE_GEMINI_ENABLED="true", LIVE_XAI_ENABLED="true", LIVE_MISTRAL_ENABLED="true")
            env.update(extra)
            result = subprocess.run([sys.executable, "-c", source], cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", timeout=15)
            return result.returncode, summary.read_text(encoding="utf-8") if summary.exists() else "", result.stderr

    def test_actual_dashboard_passes_with_collapsed_supplemental_rate_warning(self):
        code, summary, error = self.run_dashboard()
        self.assertEqual(code, 0, error)
        front, details = summary.split("<details>", 1)
        self.assertIn("Product Qualification: Passed", front)
        self.assertNotIn("not verified", front)
        self.assertIn("Mistral", details)
        self.assertIn("not verified — rate limited", details)
        self.assertNotIn("every provider passed", summary)

    def test_unenrolled_provider_needs_no_executed_job_or_outcome(self):
        def unconfigured(jobs, needs, env):
            jobs.pop()
            needs["live_mistral"] = {"result": "skipped", "outputs": {}}
            env["LIVE_MISTRAL_ENABLED"] = "false"
        code, summary, error = self.run_dashboard(unconfigured)
        self.assertEqual(code, 0, error)
        self.assertIn("not configured", summary.split("<details>", 1)[1])

    def test_actual_dashboard_blocks_missing_failed_stale_or_contradictory_proof(self):
        def mutate(kind):
            def apply(jobs, needs, env):
                if kind == "primary_rate":
                    needs["live_anthropic"]["outputs"]["evidence"] = record("anthropic", "rate_limited")
                elif kind == "mixed_defect":
                    needs["live_xai"]["outputs"]["evidence"] = record("xai", "failure")
                elif kind == "missing_record":
                    needs["live_mistral"]["outputs"] = {}
                elif kind == "stale_record":
                    needs["live_mistral"]["outputs"]["evidence"] = record("mistral", "rate_limited", run_attempt="1")
                elif kind == "contradictory_job":
                    needs["live_mistral"]["result"] = "failure"
                elif kind == "failed_job":
                    jobs[-1]["conclusion"] = "failure"; needs["live_mistral"]["result"] = "failure"
                elif kind == "missing_job":
                    jobs.pop()
                elif kind == "duplicate_job":
                    jobs.append(dict(jobs[-1]))
                elif kind == "package_failure":
                    jobs[3]["conclusion"] = "failure"
            return apply
        for kind in ["primary_rate", "mixed_defect", "missing_record", "stale_record", "contradictory_job", "failed_job", "missing_job", "duplicate_job", "package_failure"]:
            with self.subTest(kind=kind):
                code, summary, error = self.run_dashboard(mutate(kind))
                self.assertNotEqual(code, 0)
                self.assertIn("BLOCKED", summary, error)
                self.assertNotIn("Product Qualification: Passed", summary)


if __name__ == "__main__":
    unittest.main()
