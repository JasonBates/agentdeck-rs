#!/usr/bin/env python3
"""Privacy-safe staged subtitle evaluation for already-installed local models.

Only ``/api/tags`` and ``/api/chat`` are called.  This script never invokes the
Ollama CLI and therefore cannot pull a model.  It rejects port 11434 so a local
benchmark cannot accidentally use the production endpoint.  By default it writes
aggregate counts and latency only; prompt text, raw replies, and checkpoint IDs
are never written.  ``--record-accepted`` is an explicit opt-in for sanitized
case labels and accepted output text.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

HERE = Path(__file__).parent
CONFIG = json.loads((HERE / "config.json").read_text())


def endpoint_url(endpoint: str, path: str) -> str:
    parsed = urllib.parse.urlparse(endpoint)
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "::1", "localhost"}:
        raise ValueError("endpoint must be an HTTP loopback address")
    if parsed.port == 11434:
        raise ValueError("port 11434 is production; use the isolated evaluation server")
    if parsed.port is None:
        raise ValueError("endpoint must include an explicit isolated port")
    return urllib.parse.urlunparse((parsed.scheme, parsed.netloc, path, "", "", ""))


def request_json(url: str, body: dict[str, Any] | None = None, timeout: int = 90) -> dict[str, Any]:
    request = urllib.request.Request(
        url,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read())


def installed_models(endpoint: str) -> set[str]:
    tags = request_json(endpoint_url(endpoint, "/api/tags"))
    return {
        name
        for model in tags.get("models", [])
        for name in (model.get("name"), model.get("model"))
        if isinstance(name, str)
    }


def require_installed(endpoint: str, model: str) -> None:
    if model not in installed_models(endpoint):
        raise ValueError(f"configured model is not installed: {model}")


def chat(endpoint: str, model: str, prompt: str, max_tokens: int, temperature: float) -> str:
    response = request_json(
        endpoint_url(endpoint, "/api/chat"),
        {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "think": False,
            "stream": False,
            "keep_alive": "0",
            "options": {
                "temperature": temperature,
                "num_predict": max_tokens,
                "num_ctx": 4096,
                "num_gpu": 0,
            },
        },
    )
    return str(response.get("message", {}).get("content") or "").strip()


def subtitle_profile_d(checkpoint: dict[str, Any]) -> str:
    """Exact current Rust ``subtitle_job`` wording, with local checkpoint inputs."""
    goal = checkpoint.get("title") or "the session's larger goal"
    previous_reply = str(checkpoint.get("prev_reply") or "")
    context = "" if not previous_reply else (
        "\nWHAT THE AGENT SAID JUST BEFORE THIS REQUEST (context only — do not\n"
        "describe the agent's reply itself, only the work it is about):\n"
        f"{previous_reply[:900]}\n"
    )
    last_prompt = str(checkpoint.get("last_prompt") or "")
    source = (
        f"LATEST REQUEST:\n{last_prompt[:700]}"
        if last_prompt
        else f"RECENT TURNS:\n{str(checkpoint.get('recent') or '')[:1200]}"
    )
    return (
        "A working session is moving through a series of requests toward one larger goal.\n"
        "Name the single concrete step now underway in service of that goal.\n\n"
        "Two examples of the right size and shape:\n\n"
        "  GOAL: Migrate the photo library to Postgres\n"
        "  REQUEST: \"the thumbnails are all coming out rotated 90 degrees\"\n"
        "  STEP: Fix rotated thumbnails in the importer\n\n"
        "  GOAL: Tune the espresso grinder settings\n"
        "  CONTEXT: the agent had just asked for a shot pulled at a finer grind\n"
        "  REQUEST: \"ok pulled it\"\n"
        "  STEP: Read the shot time at the finer grind\n\n"
        "Notice the second one: a short answer names no work by itself, so the step comes\n"
        "from what the answer sets in motion.\n\n"
        f"THE LARGER GOAL: {goal}\n{context}{source}\n\n"
        "One action on one thing, at most 8 words. Never restate the goal. Write\n"
        "impersonally, never \"you\" or \"I\". Name the subject of the work, not the\n"
        "assistant's response to it. No quotes, no trailing period.\n\n"
        "STEP:"
    )


def tidy_subtitle(raw: str, title: str | None) -> tuple[str | None, str | None]:
    """Apply the subtitle output cleanup contract, including a leading STEP: marker."""
    text = raw.strip()
    if text[:5].upper() == "STEP:":
        text = text[5:].lstrip()
    for marker in ("LABEL:", "NAME:", "FOCUS:", "STATE:"):
        index = text.upper().find(marker)
        if index >= 0:
            text = text[index + len(marker):].lstrip()
    text = text.splitlines()[0] if text.splitlines() else text
    text = text.strip("\"'`“” ").strip(". ")
    if len(text) <= 3:
        return None, "empty-or-short"
    if len(text) > 130:
        return None, "too-long"
    words = [word for word in re.split(r"[^0-9A-Za-z]+", text.lower()) if word]
    assistant_actions = {
        "greet", "greeting", "greets", "acknowledge", "acknowledging", "acknowledges",
        "respond", "responding", "reply", "replying", "answer", "answering", "assist",
        "assisting", "help", "helping", "welcome", "welcoming", "thank", "thanking",
        "apologize", "apologise", "chat", "chatting", "converse", "conversing", "engage",
        "engaging", "introduce", "introducing",
    }
    if words and words[0] in assistant_actions:
        return None, "assistant-action"
    if title:
        title_words = {word for word in re.split(r"[^0-9A-Za-z]+", title.lower()) if len(word) > 3}
        subtitle_words = {word for word in re.split(r"[^0-9A-Za-z]+", text.lower()) if len(word) > 3}
        if title_words and len(title_words & subtitle_words) / len(title_words) >= 0.6:
            return None, "too-close-to-title"
    return text, None


JUDGE_PROMPT = """Two short labels describe what a working session is doing right now.
The REFERENCE is correct. Decide how well the CANDIDATE names the same step.

REFERENCE: {gold}
CANDIDATE: {candidate}

Score:
2 = the same step — same action on the same thing, wording may differ
1 = the right area but the wrong grain: the goal or the topic restated, or a
    different step within the same session
0 = a different step, or too vague to identify one

Answer with only the digit.

SCORE:"""


def judge(endpoint: str, model: str, candidate: str, gold: str) -> int | None:
    reply = chat(endpoint, model, JUDGE_PROMPT.format(candidate=candidate, gold=gold), 4, 0.0)
    return next((int(character) for character in reply if character in "012"), None)


def percentile(values: list[float], percentile_value: float) -> float | None:
    if not values:
        return None
    values = sorted(values)
    index = round((len(values) - 1) * percentile_value)
    return round(values[index], 3)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", choices=CONFIG["stages"], required=True)
    parser.add_argument("--profile", choices=CONFIG["profiles"], default="D-current-core")
    parser.add_argument("--endpoint", required=True, help="isolated loopback Ollama endpoint, never :11434")
    parser.add_argument("--candidate-model", required=True)
    parser.add_argument("--judge-model", help="optional installed model for 0/1/2 semantic scores")
    parser.add_argument("--judge-endpoint", help="defaults to --endpoint")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--record-accepted", action="store_true",
                        help="explicitly write accepted outputs with sanitized case labels")
    parser.add_argument("--run-id", help="safe output directory name; defaults to UTC timestamp")
    arguments = parser.parse_args()
    if arguments.repeats < 1:
        parser.error("--repeats must be at least 1")

    candidate_endpoint = arguments.endpoint.rstrip("/")
    judge_endpoint = (arguments.judge_endpoint or candidate_endpoint).rstrip("/")
    # Validate before any request and verify model tags without pulling them.
    endpoint_url(candidate_endpoint, "/api/tags")
    endpoint_url(judge_endpoint, "/api/tags")
    require_installed(candidate_endpoint, arguments.candidate_model)
    if arguments.judge_model:
        require_installed(judge_endpoint, arguments.judge_model)

    checkpoints = json.loads((HERE / CONFIG["checkpoint_source"]).read_text())
    gold = {key: value for key, value in json.loads((HERE / CONFIG["gold_source"]).read_text()).items()
            if not key.startswith("_")}
    indices = CONFIG["stages"][arguments.stage]["indices"]
    selected = checkpoints if indices == "all" else [checkpoints[index] for index in indices]
    run_id = arguments.run_id or datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", run_id):
        parser.error("--run-id may contain only letters, digits, dot, underscore, and hyphen")
    output_dir = HERE / "runs" / run_id
    output_dir.mkdir(parents=True, exist_ok=False)

    accepted_records: list[dict[str, Any]] = []
    latencies: list[float] = []
    failures: Counter[str] = Counter()
    scores: Counter[str] = Counter()
    accepted = 0
    for position, checkpoint in enumerate(selected, 1):
        safe_case = f"{arguments.stage}-{position:02d}"
        for repeat in range(1, arguments.repeats + 1):
            started = time.perf_counter()
            try:
                raw = chat(candidate_endpoint, arguments.candidate_model,
                           subtitle_profile_d(checkpoint), 32, 0.1)
            except (OSError, urllib.error.URLError, urllib.error.HTTPError, ValueError) as error:
                failures[f"transport:{type(error).__name__}"] += 1
                continue
            latency = time.perf_counter() - started
            latencies.append(latency)
            text, rejection = tidy_subtitle(raw, checkpoint.get("title"))
            if text is None:
                failures[rejection or "rejected"] += 1
                continue
            accepted += 1
            score: int | None = None
            if arguments.judge_model:
                try:
                    score = judge(judge_endpoint, arguments.judge_model, text, gold[checkpoint["id"]])
                except (OSError, urllib.error.URLError, urllib.error.HTTPError, ValueError) as error:
                    failures[f"judge:{type(error).__name__}"] += 1
                scores[str(score) if score is not None else "unscored"] += 1
            if arguments.record_accepted:
                accepted_records.append({
                    "case": safe_case,
                    "repeat": repeat,
                    "accepted": text,
                    "score": score,
                })

    total = len(selected) * arguments.repeats
    scored = sum(scores[str(value)] for value in range(3))
    score_sum = sum(value * scores[str(value)] for value in range(3))
    aggregate = {
        "schema": 1,
        "stage": arguments.stage,
        "profile": arguments.profile,
        "cases": len(selected),
        "repeats": arguments.repeats,
        "total_generations": total,
        "candidate": {"options": {"think": False, "keep_alive": "0", "num_gpu": 0,
                                     "temperature": 0.1, "num_predict": 32, "num_ctx": 4096}},
        "judge": None if not arguments.judge_model else {"used": True, "rubric": "0/1/2"},
        "acceptance": {"accepted": accepted, "rate": round(accepted / total, 4),
                       "rejections_or_errors": dict(sorted(failures.items()))},
        "candidate_latency_seconds": {
            "n": len(latencies), "median": round(statistics.median(latencies), 3) if latencies else None,
            "p95": percentile(latencies, 0.95), "mean": round(statistics.mean(latencies), 3) if latencies else None,
        },
        "semantic_score": None if not arguments.judge_model else {
            "scored": scored, "distribution": {str(value): scores[str(value)] for value in range(3)},
            "mean_of_scored": round(score_sum / scored, 3) if scored else None,
            "unscored": scores["unscored"],
            "method": "separate local judge using the committed gold labels and the displayed 0/1/2 rubric; not human-calibrated",
        },
        "privacy": {"raw_prompts_written": False, "raw_replies_written": False,
                    "checkpoint_ids_written": False, "accepted_outputs_recorded": arguments.record_accepted},
    }
    (output_dir / "aggregate.json").write_text(json.dumps(aggregate, indent=2) + "\n")
    if arguments.record_accepted:
        (output_dir / "accepted.jsonl").write_text(
            "".join(json.dumps(record) + "\n" for record in accepted_records))
    print(json.dumps(aggregate, indent=2))


if __name__ == "__main__":
    main()
