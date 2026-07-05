#!/bin/zsh
# Build one eval vault: karpathy scaffold + a raw layer copy + model config.
#
#   setup-vault.sh <vault-dir> <model-id> <raw-src-dir>
#
# Env: COGS (binary path), OMLX_API_KEY / OMLX settings fallback,
#      EMBED_MODEL / EMBED_DIM (default Qwen3-Embedding-0.6B-8bit / 1024),
#      LLM_BASE_URL (default http://localhost:8000/v1),
#      EXTRA_TOML — appended verbatim to cogs.toml (e.g. [llm.extra_body] knobs).
set -eu
VAULT=$1; MODEL=$2; RAW_SRC=$3
COGS=${COGS:-$(dirname "$0")/../../target/debug/cogs}
rm -rf "$VAULT" && mkdir -p "$VAULT" && cd "$VAULT"
"$COGS" init --karpathy >/dev/null
rsync -a --exclude '.DS_Store' "$RAW_SRC/" raw/
python3 - "$MODEL" "${EMBED_MODEL:-Qwen3-Embedding-0.6B-8bit}" "${EMBED_DIM:-1024}" "${LLM_BASE_URL:-http://localhost:8000/v1}" <<'EOF'
import re, sys
model, emb_model, emb_dim, base_url = sys.argv[1:5]
t = open('cogs.toml').read()
emb = f'''[embeddings]
enabled = true
provider = "omlx"
model = "{emb_model}"
dim = {emb_dim}
endpoint = "{base_url}"
api_key_env = "OMLX_API_KEY"
char_cap = 7000
exclude_kinds = ["source", "moc"]
query_instruction = "Given a question, retrieve wiki notes that answer it"
'''
llm = f'''[llm]
provider = "omlx"
model = "{model}"
base_url = "{base_url}"
api_key_env = "OMLX_API_KEY"
max_tokens = 8192
timeout_secs = 600
'''
t = re.sub(r'\[embeddings\].*?(?=\n\[|\Z)', emb + '\n', t, flags=re.S)
t = re.sub(r'\[llm\].*?(?=\n\[|\Z)', llm + '\n', t, flags=re.S)
open('cogs.toml','w').write(t)
EOF
[ -n "${EXTRA_TOML:-}" ] && printf '\n%s\n' "$EXTRA_TOML" >> cogs.toml
git init -q && git add -A && git commit -qm "eval vault ($MODEL)"
export OMLX_API_KEY=${OMLX_API_KEY:-$(python3 -c "import json; print(json.load(open('$HOME/.omlx/settings.json'))['auth']['api_key'])")}
"$COGS" sync >/dev/null
echo "ready: $VAULT ($MODEL)"
