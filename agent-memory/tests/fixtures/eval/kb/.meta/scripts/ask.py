#!/usr/bin/env python3
"""Synthetic `ask.py` for the `eval-recall` sample fixture.

The sample is self-contained: the knowledge base has exactly one note and one
chunk, so `kb-cache` in `queries.sample.jsonl` matches a real text range. The
stub accepts and ignores the runtime flags (`--json --no-log --top-k --decay
[--bm25] -- <query>`) and answers only the synthetic cache question.
"""

import json
import sys
from pathlib import Path

CONTRACT_VERSION = 1
ROOT = Path(__file__).resolve().parents[2]
NOTE = "notes/cache.md"
CHUNK = "缓存命中率 与 整表缓存 的口径"


def envelope(results):
    return {
        "contract_version": CONTRACT_VERSION,
        "ok": True,
        "vault_root": str(ROOT),
        "degraded": False,
        "detail": {"mode": "dense", "escalated": []},
        "results": results,
    }


def main(argv):
    query = ""
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == "--":
            if index + 1 < len(argv):
                query = argv[index + 1]
            break
        if token in ("--top-k", "--decay"):
            index += 2
            continue
        if not token.startswith("-"):
            query = token
        index += 1
    results = []
    if "缓存" in query:
        results = [
            {
                "path": NOTE,
                "score": 0.9,
                "chunk_index": 0,
                "chunk_text": CHUNK,
            }
        ]
    sys.stdout.write(json.dumps(envelope(results), ensure_ascii=False))
    sys.stdout.write("\n")
    sys.stdout.flush()
    raise SystemExit(0)


if __name__ == "__main__":
    main(sys.argv[1:])
