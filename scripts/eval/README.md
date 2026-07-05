# Model eval harness — student vs teacher over real ingests

Compares two `[llm]` models by running the *same* raw-capture sample through
`cogs ingest` in twin fresh vaults, then diffing objective metrics: completion,
speed, JSON reliability, validator-warning profile, and page content stats.
This is how `lewisdog/qwen3-1.7b-cogs-ingest` was accepted as a teacher
replacement for extraction/summarisation (2026-07-05).

```sh
# 1. Sample: every 13th raw file, chronological (adjust stride to taste)
cd <source-vault>
find raw -name '*.md' ! -name 'README.md' ! -name '*.meta.md' \
  | awk -F/ '{print $NF "|" $0}' | sort | cut -d'|' -f2 \
  | awk 'NR % 13 == 5' > /tmp/sample.txt

# 2. Twin vaults (same raw layer, different [llm].model)
scripts/eval/setup-vault.sh /tmp/eval-student <student-model> <source-vault>/raw
scripts/eval/setup-vault.sh /tmp/eval-teacher <teacher-model> <source-vault>/raw

# 3. Run both passes (sequential keeps the GPU honest), one combined log
scripts/eval/run-pass.sh /tmp/eval-student /tmp/sample.txt  > /tmp/runlog.txt
echo "=== TEACHER ===" >> /tmp/runlog.txt
scripts/eval/run-pass.sh /tmp/eval-teacher /tmp/sample.txt >> /tmp/runlog.txt

# 4. Compare (expects <base>/eval-student and <base>/eval-teacher)
python3 scripts/eval/analyze.py /tmp /tmp/runlog.txt
```

Serving configs matter as much as weights — pin them via `EXTRA_TOML`:

```sh
# the fine-tuned student runs away at pure greedy — give it a penalty
EXTRA_TOML=$'[llm.extra_body]\nrepetition_penalty = 1.1' \
  scripts/eval/setup-vault.sh /tmp/eval-student qwen3-1.7b-cogs-ingest-8bit <raw>

# Qwen3.6 teachers think for thousands of hidden tokens unless told not to
EXTRA_TOML=$'[llm.extra_body.chat_template_kwargs]\nenable_thinking = false' \
  scripts/eval/setup-vault.sh /tmp/eval-teacher Qwen3.6-35B-A3B-UD-MLX-4bit <raw>
```

Reading the analyzer output: `unparseable replies` is raw JSON reliability
(before tolerant-parse salvage); the warning profile shows what the validators
had to police — hallucinated links and claim rewrites are model-quality
signals, dropped quotes are fabrication/paraphrase, and a flood of anything
usually means a decoding problem, not a weights problem.
