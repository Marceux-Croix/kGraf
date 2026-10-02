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
| `long-run-one-file.txt` | eight classes in one file | eight chained contracts that all write `shapes/library.py`, checked by one test file |

The order-pricing task is a Cargo workspace with one crate per contract; the
long-run task is a Python package with one module and one test file.
