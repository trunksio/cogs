#!/usr/bin/env python3
"""LoRA SFT for the cogs ingest student model, on a `cogs distill` dataset.

CUDA path (DGX Spark / any NVIDIA box) using TRL + PEFT. The dataset is the
directory `cogs distill` writes: train.jsonl / valid.jsonl in chat format
({"messages": [{role, content}, ...]}), assistant turn = compact JSON target.

The Apple-silicon equivalent is one command, no script:
  python3 -m mlx_lm.lora --model <mlx base> --train --data <dataset dir>

NOTE for operators: TRL renames SFTConfig fields between versions
(max_length/max_seq_length, eval_strategy/evaluation_strategy,
processing_class/tokenizer). If the installed version rejects an argument,
adapt the name — the intent of each setting is what matters.
"""
import argparse

from datasets import load_dataset
from peft import LoraConfig
from transformers import AutoModelForCausalLM, AutoTokenizer
from trl import SFTConfig, SFTTrainer


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--model", default="Qwen/Qwen3-1.7B")
    p.add_argument("--data", required=True, help="dir containing train.jsonl/valid.jsonl")
    p.add_argument("--out", default="out-lora")
    p.add_argument("--epochs", type=float, default=2.0)
    p.add_argument("--max-seq", type=int, default=8192)
    p.add_argument("--batch", type=int, default=2)
    p.add_argument("--grad-accum", type=int, default=8)
    p.add_argument("--lr", type=float, default=1e-4)
    p.add_argument("--smoke", action="store_true", help="100 steps to measure throughput")
    args = p.parse_args()

    ds = load_dataset(
        "json",
        data_files={"train": f"{args.data}/train.jsonl", "eval": f"{args.data}/valid.jsonl"},
    )

    peft_cfg = LoraConfig(
        r=16,
        lora_alpha=32,
        lora_dropout=0.05,
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj"],
        task_type="CAUSAL_LM",
    )

    cfg = SFTConfig(
        output_dir=args.out,
        num_train_epochs=args.epochs,
        max_steps=100 if args.smoke else -1,
        per_device_train_batch_size=args.batch,
        gradient_accumulation_steps=args.grad_accum,
        learning_rate=args.lr,
        lr_scheduler_type="cosine",
        warmup_ratio=0.03,
        logging_steps=10,
        eval_strategy="steps",
        eval_steps=200,
        save_steps=200,
        save_total_limit=2,
        bf16=True,
        max_length=args.max_seq,
        packing=False,
        gradient_checkpointing=True,
        report_to=[],
    )

    model = AutoModelForCausalLM.from_pretrained(
        args.model, torch_dtype="bfloat16", attn_implementation="sdpa"
    )
    tok = AutoTokenizer.from_pretrained(args.model)

    trainer = SFTTrainer(
        model=model,
        args=cfg,
        train_dataset=ds["train"],
        eval_dataset=ds["eval"],
        processing_class=tok,
        peft_config=peft_cfg,
    )
    trainer.train()
    trainer.save_model(args.out)
    print(f"adapter saved to {args.out}")


if __name__ == "__main__":
    main()
