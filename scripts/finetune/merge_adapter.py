#!/usr/bin/env python3
"""Merge a LoRA adapter into its base model (HF format) so the result can be
converted for local serving — e.g. on Apple silicon:
  python3 -m mlx_lm.convert --hf-path merged/ -q --mlx-path <name>
then drop into ~/.omlx/models and point cogs [llm] at it.
"""
import argparse

from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--base", default="Qwen/Qwen3-1.7B")
    p.add_argument("--adapter", required=True)
    p.add_argument("--out", default="merged")
    args = p.parse_args()

    model = AutoModelForCausalLM.from_pretrained(args.base, torch_dtype="bfloat16")
    model = PeftModel.from_pretrained(model, args.adapter)
    model = model.merge_and_unload()
    model.save_pretrained(args.out)
    AutoTokenizer.from_pretrained(args.base).save_pretrained(args.out)
    print(f"merged model at {args.out}")


if __name__ == "__main__":
    main()
