# Agent regression cases

For bounded instruction/skill refinement with a held-out adoption gate, see
[offline harness experiments](experiments.md). It reuses the evaluator described
here and preserves the baseline when improvement cannot be established.

A case supplies an input and a JSON Schema for the expected final output. Use
`const` for an exact JSON value, or ordinary schema constraints for a broader
contract. The expected schema stays outside the agent's context and does not
replace its configured goal or runtime verification.

```json
[
  {
    "name": "mean-of-three-values",
    "input": "Return only JSON with the mean of 10, 20, and 30 as {\"mean\":number}.",
    "expected_schema": {
      "type": "object",
      "required": ["mean"],
      "properties": {"mean": {"const": 20}}
    }
  }
]
```

Save that as `cases.json` and run:

```sh
hudson-worker --config agent-v1.json --evaluate cases.json
hudson-worker --config agent-v1.json --evaluate cases.json \
  --compare-config agent-v2.json
```

Use different immutable agent versions when changing definitions. Both
configurations are loaded and built before evaluation dispatch begins. Each case
gets a fresh Run through the same `AgentLoop`, policies, tools, goals, budgets,
and output verification as normal execution. Add `--database` and `--namespace`
to retain those runs and their events in PostgreSQL.

The JSON report contains the suite digest, each agent reference, per-case run ID,
status, result, admitted call counts and available provider token reports, runtime assessment, and independent evaluation result. With
a comparison, exit status reflects the candidate; without one, it reflects the
single configuration. A completed run can fail its regression contract. Waiting,
failed, cancelled, or uncertain runs never pass. Approvals are not granted by the
evaluator, and interrupted work is not replayed.

Cases are limited to 100 per suite, unique names, and a 1 MiB suite file in the CLI.
Agent limits still apply to every run. Shared model-call groups remain cumulative
across cases and versions; configure separate groups when comparing independent
budgets. Reports identify cases by content digest, but do not make nondeterministic
providers reproducible. Control model/tool responses when reproducibility matters.

Tools run normally. Point configurations at isolated test services; the evaluator
does not clone an external system or make production effects harmless. Schema
checks assess output shape/content, not arbitrary business outcomes. Library users
can inspect the returned `Report` and its run IDs for additional application checks.

## Offline example

```sh
cargo build --locked -p hudson-worker
python3 scripts/smoke_evaluation.py
```

This uses a loopback model stub and two versions of one agent. The baseline
finishes but produces an incorrect answer; the candidate passes the same case.
It checks that expected criteria never reach the model, malformed suites fail
before dispatch, and reports retain ordinary runtime results and usage. No model
credentials or paid requests are used.

### Value criteria and execution evidence

Goals and evaluation cases optionally accept `criteria`, a list of deterministic
checks. Supported forms are `{"type":"equals","pointer":"/answer","expected":42}`,
`{"type":"number_range","pointer":"/score","min":0.8,"max":1.0}`, and
`{"type":"contains","pointer":"/summary","text":"required phrase"}`. Pointers
use JSON Pointer syntax. Missing values and wrong types fail checks.

Goal criteria are visible to the agent and participate in runtime verification:
a failure returns feedback through the normal bounded repair loop. Evaluation
criteria remain held out, and can reject an otherwise completed run. These
checks establish only their stated predicates, not general factual accuracy.

Assessments include `evidence` references to successful recorded tool operations,
with operation IDs and request/result digests. These provide auditable provenance;
a successful tool execution alone does not prove that a final claim is true.
Evaluation cases also report elapsed wall-clock milliseconds alongside the run's
recorded usage. Timing includes waits encountered while driving that case.

For checks implemented by customer tools, use `tool_result_equals`:

```json
{"type":"tool_result_equals","tool_name":"run_tests","require_latest_tool":true,"arguments":{"suite":"acceptance"},"pointer":"/passed","expected":true}
```

This requires the latest matching tool execution in this run to have succeeded
and its recorded output to contain the expected value. A model assertion cannot
satisfy it; a newer failed or uncertain execution prevents an older passing result
from satisfying it. Omit `arguments` to match any arguments. The tool remains an
ordinary authorized, budgeted operation. The tool owner must ensure that its result
verifies the relevant artifact/version; Hudson does not infer that relationship.

Set `require_latest_tool` to true when verification must follow all other tool
work (for example, testing after file edits). Any later tool operation invalidates
that check. It defaults to false for historical receipt checks.
