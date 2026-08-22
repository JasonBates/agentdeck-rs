# Synthetic local-model subtitle harness

This public harness exercises AgentDeck's subtitle prompt and output cleanup with
ten synthetic fixtures. It is a reproducibility and safety check, not evidence for
a model recommendation or product-quality score. It never searches home directories,
opens local transcripts, reads Copilot/Claude/Codex state, invokes an Ollama CLI, or
pulls a model.

The harness accepts only an explicit isolated loopback Ollama endpoint and rejects
production port `11434`. Calls use `num_gpu=0` and `keep_alive=0`. The candidate and
optional judge model must already be installed; `/api/tags` verifies that condition.

By default each run writes only ignored aggregate counts and latency. It never writes
prompts, raw replies, source identifiers, endpoint paths, or model names. The explicit
`--record-accepted` option writes accepted output keyed only by synthetic case position;
treat that ignored artifact as local data.

## Run

```bash
cd Evals/local-models
python3 -m unittest test_run.py test_public_hygiene.py
python3 run.py --stage smoke10 --endpoint http://127.0.0.1:11435 \
  --candidate-model your-already-installed-model
```

An optional local judge can produce the displayed 0/1/2 rubric counts, but those are
diagnostic only. Do not use this synthetic corpus by itself to select a default product
model. Local enrichment is AgentDeck's recommended full experience; Herdr-only mode is
the graceful fallback for machines where a suitable model is unavailable.

## Fixture rules

Fixtures are public source. Keep them fictional, short, and free of real prompts,
replies, transcript/screen text, paths, session identifiers, provider information,
credentials, or secrets. `test_public_hygiene.py` enforces the public fixture shape,
allows only the documented harness files, and blocks home-directory discovery markers.
