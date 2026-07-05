# cogs student-model fine-tune — DGX Spark runbook

You are (probably) a Claude Code session on `spark-a2ae` (NVIDIA DGX Spark,
GB10, 128GB unified memory, aarch64). Your job: LoRA-tune **Qwen/Qwen3-1.7B**
on the cogs ingest dataset in `~/cogs-finetune/dataset/`, verify it, and leave
the artifacts where the Mac can fetch them. A human (Lewis) is reachable but
assume you run autonomously.

## Context (why this exists)

cogs (github.com/trunksio/cogs) is a graph wiki engine whose `cogs ingest`
pipeline currently uses a large teacher LLM for four tasks: `extract`
(raw doc → summary/claims/quotes/entities JSON), `suggest_links` (weave
wikilinks into claims), `page_update` (append-only page sections), and
`contradiction` (conservative conflict detection). The dataset here was
produced by `cogs distill` from 256 real ingests: 1,921 train / 192 valid
chat-format pairs, assistant turn = compact JSON exactly as the runtime
parses it. The fine-tuned student replaces the teacher as a local
OpenAI-compatible provider — so **output-format fidelity (strict JSON,
schema-exact) matters more than eloquence**.

## Machine constraints — read first

- `ds4-server` (DeepSeek V4 Flash, `/home/lewis/work/deepseekv4/ds4/ds4-server`,
  port 8000) normally occupies ~108GB of unified memory. **Training needs that
  memory: stop it before the run and RESTART it after** — it is Lewis's
  serving instance. Find how it's launched (systemd unit, tmux, docker?) with
  `systemctl list-units | grep -i ds4`, `ps -o args= -p $(pgrep ds4-server)`,
  etc., and note the restart command **before** stopping it. If you cannot
  determine how to restart it, ask Lewis before stopping it.
- Verify free memory after stopping (`free -g`): expect ~115GB available.
- Ollama also listens on 11434; it lazy-loads and shouldn't hold memory. Leave it.

## Setup

```sh
cd ~/cogs-finetune
python3 -m venv venv && . venv/bin/activate
pip install --upgrade pip
pip install torch --index-url https://download.pytorch.org/whl/cu130  # aarch64 CUDA wheel; fall back to plain `pip install torch` if the index 404s
pip install transformers trl peft datasets accelerate
python -c "import torch; print(torch.cuda.is_available(), torch.cuda.get_device_name(0))"
```

Must print `True NVIDIA GB10`. If torch can't see CUDA, the NGC PyTorch
container is the fallback (`docker run --gpus all -v ~/cogs-finetune:/w
nvcr.io/nvidia/pytorch:<latest> ...`).

## Run

1. **Smoke (measure, ~5 min):**
   ```sh
   python train_lora.py --data dataset --smoke --out out-smoke
   ```
   Watch tokens/sec + memory. Compute ETA: ~4M tokens/epoch. If OOM at
   `--max-seq 8192 --batch 2`, drop to `--batch 1 --grad-accum 16`.
   TRL renames SFTConfig args across versions — fix names per the note in
   train_lora.py if it rejects any.

2. **Full run (background, ~2 epochs):**
   ```sh
   nohup python train_lora.py --data dataset --out out-lora > train.log 2>&1 &
   ```
   Healthy: train loss dropping from ~1.x and eval loss decreasing at each
   eval step (200). Alarming: eval loss rising after epoch 1 (stop at the
   best checkpoint — save_total_limit keeps the last two).

3. **Merge for serving:**
   ```sh
   python merge_adapter.py --adapter out-lora --out merged
   ```

4. **Sanity eval (do not skip):** generate on 5 valid.jsonl prompts (strip the
   assistant turn, generate up to 2048 tokens) and check each reply parses as
   JSON with the expected top-level keys (`summary`/`key_claims`… for extract;
   `linked_claims`… for suggest_links; `findings` for contradiction;
   `topic`/`section_md`/`relevant` for page_update). Report the parse rate —
   this is the success metric, not loss. A quick harness:
   `transformers` `pipeline("text-generation", model="merged")` with the chat
   messages minus the last turn, temperature 0.

5. **Restart ds4-server.** Verify: `curl -s http://100.67.178.71:8000/v1/models | head -c 100`.

## Deliverables

Leave in `~/cogs-finetune/`: `out-lora/` (adapter), `merged/` (HF model),
`train.log`, and a short `RESULTS.md` (tokens/sec, final train/eval loss,
JSON-parse rate on the 5-sample eval, anything surprising). The Mac side
fetches `merged/` and converts with `mlx_lm.convert -q` for omlx serving.
