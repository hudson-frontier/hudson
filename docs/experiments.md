# Offline harness experiments

The worker can run one bounded refinement step around a frozen agent template.
It creates immutable trial versions, probes instruction/skill edits, composes a
candidate, and compares it with the baseline on held-out cases. Selection returns
a recommendation and typed patch; it does not replace an active agent or edit the
source configuration. The original goal and output contract still apply.

## Run the fixture pilot

```sh
cargo build --locked -p hudson-worker
python3 scripts/smoke_experiments.py
```

This starts local scripted model/tool fixtures and runs the real worker. It needs
no provider credentials. The deterministic fixture establishes the control flow,
not a quality gain for a real model. Set `HUDSON_TEST_DATABASE` to a dedicated local
database to include process restart, exact resume, inspection, approval and
cancellation checks. The full checker includes those checks with `--database`.

## Supply a template and spec

Use a trusted loopback fixture or local compatible model endpoint. The template
must select an explicit model and a literal `127.0.0.1` or `[::1]` HTTP(S) endpoint.
Read-only HTTP tools must also use literal loopback addresses. Experiment builds
omit model credentials even when a provider key exists in the host environment.
Model and tool fixture clients also disable inherited HTTP proxy settings.
That credential/proxy policy is part of the model transport binding, so an
ordinary executor cannot silently replace an experiment's fixture executor.

```json
{
  "request_key": "labels-pilot-1",
  "domain": "labels",
  "probe": "Label the visible example",
  "max_model_calls": 32,
  "validation": [{
    "name": "held-out-example",
    "input": "Label another example",
    "expected_schema": {"const": "expected label"}
  }]
}
```

```sh
hudson-worker --config agent.json --experiment experiment.json --experiment-plan
hudson-worker --database hudson_test --namespace offline-pilot \
  --config agent.json --experiment experiment.json --experiment-ticks 10
hudson-worker --database hudson_test --namespace offline-pilot \
  --config agent.json --experiment experiment.json
hudson-worker --database hudson_test --namespace offline-pilot --inspect-experiment EXPERIMENT_UUID
hudson-worker --database hudson_test --namespace offline-pilot --cancel-experiment EXPERIMENT_UUID
```

`--experiment-plan` validates the template, spec, schemas and frozen skill files
without opening a database, reading credentials or calling providers/tools.
`--experiment-ticks` limits one invocation; repeating the same command resumes
the persisted stage. Without PostgreSQL, state lasts only for that process.
Inspection and cancellation require no config file or fixture endpoint.

The baseline probe runs first. Separate general and task-specific proposal runs
each produce an instruction/skill patch and a probe execution. A composition run
reads those contrasts and produces a third candidate, which is also probed.
Reflection then produces a bounded lesson referencing visible probe run IDs.
Only afterward do fresh baseline and composed-candidate runs execute the held-out
inputs. The selector uses the ordinary evaluator, without a model call.

Proposals and composition return JSON of this form:

```json
{"instructions":"Revised instructions","skills":{"existing_skill_name":"Revised body"}}
```

Only existing skill bodies can change; names and descriptions remain fixed.
Provider/model settings, tool schemas/endpoints, policies, approval requirements,
limits, input/output contracts and goals are copied from the template. Internal
editing agents have no tools. Their context contains the visible task, fixed
contract, proposed changes, complete probe outputs, tool status/digest summaries,
and up to four pinned prior lessons. Oversized context fails explicitly; no hidden
summarization calls or silent output truncation are used. Original operation
records retain the source evidence. Provider thinking/continuation metadata is
not supplied as optimization evidence.

Held-out schemas, criteria, inputs and verdicts are excluded from editing and
reflection context. Solver runs receive only their own case input. Reflection
finishes before held-out evaluation, so its lesson cannot read grading results.
The candidate must pass more cases than the baseline and retain every baseline
pass. A tie or regression selects the baseline. Incomplete executions are
unscored and cannot be selected as improvements. A completed experiment may
legitimately retain the baseline; that is not an execution failure.

Completed JSON `null` results remain valid after PostgreSQL reload. The public
projection uses the exact verified-output digest to distinguish a real null
completion from a missing result. Missing results without that receipt are not
assigned a quality score and cannot support adoption.

## Durability, authority and bounds

The manifest pins the exact template, probe, held-out suite, budget, actor and
prior-lesson snapshot by SHA-256 digest. A reused key with changed content
conflicts, including after completion. New tasks can retrieve only already
retained lessons; a running experiment never refreshes its snapshot. Domain,
template, workspace and actor scope lesson reuse. Lessons remain inferences with
source run IDs, not trusted instructions or proof of general correctness.

All actual model requests share one persisted admission counter, including
proposal/composition/reflection calls and normal verification repair. Failures
and uncertain requests keep their charges. There are no extra model-based
selection calls, automatic retries, budget refunds or provider switches.

Each stage uses a normal durable run. Stage admission and registration commit
together; concurrent drivers find the same run. A late dispatch of an already
settled operation is ignored after checking run ownership. For active runs,
pending operations still require current batch membership, preserving authorization
and preventing duplicate attempts. Dispatch validates the operation's existence and run ownership
even when the run has stopped; a missing or foreign operation remains an error.
An owned operation on a terminal or cancelling run remains a no-op.
Queue steering/follow-ups are
rejected for stage runs because their inputs are frozen. Ordinary policy and
approval checks still gate read tools. An approval wait or uncertain operation
pauses the experiment. Inspect its stage run and use the existing approval or
trusted reconciliation controls before resuming. Cancellation fences every
admitted run atomically and prevents later stage admission. It retains a
`cancelling` result while any external outcome remains unresolved.

| Resource | Limit |
| --- | --- |
| Refinement steps | Exactly one |
| Probe task | One input, at most 4 KiB serialized |
| Held-out cases | 1–8, unique inputs distinct from the probe |
| Editing runs | Four: general, specific, composition, reflection |
| Total stage runs | `8 + 2 * validation_cases`, at most 24 |
| Shared model requests | Explicit 1–256 ceiling; includes failed/uncertain admissions |
| Per editing run | At most two model requests, subject to lower template limits |
| Spec / frozen template | At most 128 KiB each |
| Candidate instructions / body | At most 16 KiB / 4 KiB per skill, 32 KiB total patch |
| Skills / read-tool bindings | At most eight / sixteen |
| Tool evidence | At most sixteen operation summaries per probe |
| Editing context | At most 64 KiB and the template's lower limits |
| Prior lessons | At most four, at most 4 KiB text each |
| Local tick batch | At most 16,384 ticks per CLI invocation |

Templates reject credentials, preexisting shared budget groups, external
destinations, memory tools, teams, MCP, user-input capabilities and skill packages.
Inline skills and explicitly loaded Markdown skill files are supported. These
restrictions bound the first experiment surface. Loopback addressing is not a
hostile-code isolation boundary; the host must own its fixture services. Real
sandbox isolation acceptance remains tracked separately in issue #17.

Experiments promote their PostgreSQL namespace to storage format 3, atomically
with the manifest. Current binaries read formats 1–3. Older writers reject 3
instead of dropping experiment state. Promotion is sticky. Upgrade every writer
first, or use a dedicated experiment namespace. Unknown experiment protocols are
rejected; the current manifest/stage protocol is version 1. Existing namespaces
that never use experiments retain their previous storage format.

## Resource comparison

A driver retains at most eight configured runtimes, one for each immutable trial
or editing agent, and one parsed template. This avoids repeating configuration
publication and HTTP client setup on every tick. Rust hosts can call
`Driver::release_resources` while idle; durable progress and admission charges
are retained.

```sh
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  CARGO_BUILD_JOBS=2 python3 scripts/benchmark_experiments.py
```

This alternates retained resources with resource release after every tick. Both
modes use the same executable, one visible task, three held-out cases, fourteen
runs and exactly twenty-four local model requests per experiment. Three rounds
produce fifteen timing samples per mode. The report pins runtime-source and
executable digests and rejects source changes during measurement. It measures
local fixture overhead, not provider latency or model quality.
