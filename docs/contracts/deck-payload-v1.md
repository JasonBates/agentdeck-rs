# DeckPayload v1 wire rules

`DeckPayload` is the browser-facing JSON contract shared by snapshots and SSE frames.

- Missing optional values are omitted when encoding. Decoding accepts an absent key or
  JSON `null`, then emits the canonical omitted form.
- Object member order has no semantic meaning. Numerically equivalent JSON spellings are
  equal, while array order remains behaviorally significant.
- Rust serialization is deterministic for runtime no-op suppression.
- Unknown capability states are not invented; absent data remains absent rather than
  becoming zero or a plausible fake.

`Tests/fixtures/contract/deck-payload-v1-sanitized.json` is a completely synthetic
contract sample. It does not invoke a live bridge or contain transcript, machine,
repository, provider-account, or model data.

The schema at `schemas/deck-payload-v1.schema.json` uses JSON Schema Draft 2020-12.
Optional properties are absent from `required` and accept only their non-null type when
present, matching canonical output.

The additive `capabilities` object reports headings, capacity, host telemetry, local-model
telemetry, and tab-title synchronization as `available`, `missing`, `disabled`,
`unsupported`, or `error`. A missing supported provider may include one AgentDeck-owned
setup hint. Disabled and unsupported features do not produce installation prompts.
