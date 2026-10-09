# Recorded planner answers

Each file is one answer a planner model gave kGraf, verbatim: the message
content of the chat completion, recorded by a live run against a local model
(`qwen3.6:35b` through Ollama, 2026-10-02). The tests feed them back through
the real plan intake, so a fix is checked against what a model actually wrote,
not against an answer written to fit the fix.

| File | Task | What it shows |
| --- | --- | --- |
| `d1-ask1-leaked-draft.txt` | order pricing, first ask | a reasoning draft, cut off with `...`, then a literal `</think>`, then the plan |
| `d1-ask3-consumer-ids.txt` | order pricing, third ask | contract ids in `consumer`, commands as lists of lists, no `tests` |
| `d6-replan-unclosed-string.txt` | order pricing, re-plan | a file's `content` string never closed, so a line break falls inside it (line 107) |
| `d8-ask3-empty-shells.txt` | order pricing, third ask | empty `reference` and `tests`, and the producer's file in both `stub` and `consumer` |
| `d1-contracts-order-pricing.txt` | order pricing, contracts ask | six contracts over the scaffolded workspace: c4 depends on c3, c5 on c1 to c4, c6 on c5; the shape every interface ask of the benchmark ran on |
| `tb-pypi-contracts.txt` | Terminal-Bench `pypi-server`, contracts ask (2026-10-08) | three contracts: write the `vectorops` package, build its wheel and source archive (`.whl`, `.tar.gz` only), serve and install them |
| `tb-pypi-ask1.txt` to `tb-pypi-ask3.txt` | the same run, the three interface asks | test files that define `def test_...` and never call them, run as `python test_x.py`; the build contract's interface with no stub, then text placeholders for the wheel |
| `tb-pypi-script-contracts.txt`, `tb-pypi-script-ask1.txt` to `tb-pypi-script-ask3.txt` | Terminal-Bench `pypi-server`, a second run (2026-10-09, `qwen3.6:35b`) | three contracts (a package, a server script, a verify script); three interface asks of 86 s, 94 s and 102 s: consumer scripts named by path alone, then a compile command that builds no test file, then consumer entries as bare path strings. The tests replay them for the ask count and the wording of the first re-ask. The commands use `python3`, `bash` and `sh`. The tests blank the contracts' `tools` lists, which the test rig does not offer |
| `long-run-one-file.txt` | eight classes in one file | eight chained contracts that all write `shapes/library.py`, checked by one test file |
| `greenfield-writeless.txt` | greenfield `wcl` workspace, second contracts ask (2026-10-09) | four contracts whose instructions create `Cargo.toml` and two crates, every `"writes"` empty, after the first answer was refused for an unwritten workspace member. Not the verbatim answer: the contracts as kGraf's trace recorded them when it accepted them (no field it does not read survives) |

The order-pricing task is a Cargo workspace with one crate per contract; the
long-run task is a Python package with one module and one test file.

The `tb-pypi-*` answers were recorded inside a Terminal-Bench container, where `python` is the
interpreter. The test that serves them replaces the program `python` in their commands with the
host's interpreter, and nothing else: a check whose program is missing does not run, and the
interface would be accepted unchecked.
