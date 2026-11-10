# Public-benchmark goals

Sampled once (seed `20261110`, stratified by topic / first reasoning type) for the 2026-11-10 Haiku 4.5 vs 5.5 eval:

- `simpleqa.json`: 20 of the 1,000 questions of [google/simpleqa-verified](https://huggingface.co/datasets/google/simpleqa-verified) (MIT), split `eval`.
- `frames.json`: 15 of the 824 questions of [google/frames-benchmark](https://huggingface.co/datasets/google/frames-benchmark) (Apache-2.0), split `test`.

Each goal has one `core` nugget: the dataset's reference answer, not re-verified here. Load with `--public tests/golden_public` (`docs/eval.md`).
