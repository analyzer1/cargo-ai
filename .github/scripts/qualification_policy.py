"""Validate sanitized probe evidence and apply live qualification requirements."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

PRIMARY = frozenset({"openai", "anthropic"})
SUPPLEMENTAL = frozenset({"gemini", "xai", "mistral"})
PROVIDERS = PRIMARY | SUPPLEMENTAL
FIELDS = {"schema_version", "candidate", "provider", "run_id", "run_attempt", "probe_id", "outcome"}
OUTCOMES = {"pass", "rate_limited", "failure"}


class EvidenceError(ValueError):
    pass


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceError("duplicate evidence field")
        result[key] = value
    return result


def parse_record(raw):
    if not isinstance(raw, str) or len(raw) > 2048:
        raise EvidenceError("missing or oversized probe evidence")
    try:
        value = json.loads(raw, object_pairs_hook=unique_object)
    except (ValueError, TypeError) as error:
        raise EvidenceError("invalid probe evidence") from error
    if not isinstance(value, dict) or set(value) != FIELDS:
        raise EvidenceError("unexpected probe evidence fields")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise EvidenceError("unsupported probe evidence version")
    if not isinstance(value["candidate"], str) or not re.fullmatch(r"[0-9a-f]{40}", value["candidate"]):
        raise EvidenceError("invalid candidate identity")
    if not isinstance(value["provider"], str) or not isinstance(value["outcome"], str) or value["provider"] not in PROVIDERS or value["outcome"] not in OUTCOMES:
        raise EvidenceError("unknown provider or probe outcome")
    for name in ("run_id", "run_attempt"):
        if not isinstance(value[name], str) or not re.fullmatch(r"[1-9][0-9]*", value[name]):
            raise EvidenceError("invalid workflow identity")
    if not isinstance(value["probe_id"], str) or not re.fullmatch(r"[0-9a-f]{32}", value["probe_id"]):
        raise EvidenceError("invalid probe identity")
    return value


def evaluate(provider, raw, *, candidate, run_id, run_attempt, job="success", enrollment="true", probe_id=None):
    if provider not in PROVIDERS:
        raise EvidenceError("unknown qualification provider")
    required = provider in PRIMARY
    if not required and enrollment not in {"", "false", "true"}:
        raise EvidenceError("invalid provider enrollment")
    if not required and enrollment in {"", "false"}:
        if job != "skipped" or raw:
            raise EvidenceError("unenrolled provider unexpectedly executed")
        return {"status": "not_configured", "accepted": True, "policy": "supplemental"}
    if job != "success":
        raise EvidenceError("provider qualification job did not complete successfully")
    record = parse_record(raw)
    for key, expected in {"provider": provider, "candidate": candidate, "run_id": run_id, "run_attempt": run_attempt}.items():
        if record[key] != str(expected):
            raise EvidenceError("probe evidence identity mismatch")
    if probe_id is not None and record["probe_id"] != probe_id:
        raise EvidenceError("stale probe evidence")
    outcome = record["outcome"]
    accepted = outcome == "pass" or (not required and outcome == "rate_limited")
    return {
        "status": "pass" if outcome == "pass" else "unverified" if outcome == "rate_limited" else "fail",
        "accepted": accepted,
        "policy": "required" if required else "supplemental",
    }


def summary_providers(records, jobs, enrollments, *, candidate, run_id, run_attempt, job_attempts=None):
    if set(records) != PROVIDERS or set(jobs) != PROVIDERS:
        raise EvidenceError("incomplete provider set")
    if not re.fullmatch(r"[1-9][0-9]*", str(run_attempt)):
        raise EvidenceError("invalid aggregate attempt")
    if job_attempts is not None:
        if set(job_attempts) != PROVIDERS or any(
            not re.fullmatch(r"[1-9][0-9]*", str(value)) or not 1 <= int(value) <= int(run_attempt)
            for value in job_attempts.values()
        ):
            raise EvidenceError("invalid producer attempts")
    seen = set()
    results = {}
    for provider in sorted(PROVIDERS):
        raw = records[provider]
        if raw:
            probe_id = parse_record(raw)["probe_id"]
            if probe_id in seen:
                raise EvidenceError("duplicate probe identity")
            seen.add(probe_id)
        results[provider] = evaluate(
            provider, raw, candidate=candidate, run_id=run_id, run_attempt=(job_attempts or {}).get(provider, run_attempt),
            job=jobs[provider], enrollment=enrollments.get(provider, ""),
        )
    return results


def write_output(name, value):
    # Only fixed names and schema-validated single-line values reach Actions output.
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"{name}={value}\n")


def run_probe(provider):
    if provider not in PROVIDERS:
        raise EvidenceError("unknown provider")
    candidate = os.environ["CARGO_AI_SHA"]
    if subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() != candidate:
        raise EvidenceError("checkout does not match exact candidate")
    # A fresh private directory prevents reuse of previous attempts or evidence.
    with tempfile.TemporaryDirectory(prefix="provider-qualification-", dir=os.environ["RUNNER_TEMP"]) as directory:
        result = Path(directory) / "result.json"
        probe_id = os.urandom(16).hex()
        environment = dict(os.environ, CARGO_AI_QUALIFICATION_REPORT=str(result), CARGO_AI_QUALIFICATION_PROBE=probe_id)
        command = ["cargo", "test", "--locked", "--test", "provider_smoke",
                   f"live_{provider}_smoke_uses_isolated_stdin_credentials", "--", "--ignored", "--exact"]
        completed = subprocess.run(command, env=environment, check=False)
        if completed.returncode != 0:
            raise EvidenceError("probe harness failed; no availability exemption applies")
        raw = result.read_text(encoding="utf-8")
        record = parse_record(raw)
        decision = evaluate(provider, raw, candidate=candidate,
                            run_id=os.environ["GITHUB_RUN_ID"], run_attempt=os.environ["GITHUB_RUN_ATTEMPT"],
                            probe_id=probe_id)
        if not decision["accepted"]:
            raise EvidenceError("live probe did not satisfy qualification policy")
        write_output("evidence", json.dumps(record, separators=(",", ":"), sort_keys=True))
        if decision["status"] == "unverified":
            print(f"::warning title=Supplemental provider detail::{provider} live verification unavailable: rate limited")
        return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=sorted(PROVIDERS))
    args = parser.parse_args()
    try:
        return run_probe(args.provider)
    except (EvidenceError, OSError, UnicodeError, KeyError, subprocess.SubprocessError):
        # Never echo raw evidence, environment, process output or provider bodies.
        print("Provider qualification failed: required probe/evidence did not satisfy policy.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
