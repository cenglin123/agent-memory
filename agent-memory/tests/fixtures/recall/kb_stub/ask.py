#!/usr/bin/env python3
"""Offline `ask.py` stand-in for recall tests (P2 KB adapter fixture).

The test harness copies this file to `<temporary kb.root>/.meta/scripts/ask.py`
and selects a branch through the configured `interpreter.kb.env`
(`{"KB_STUB": "<branch>"}`); recall appends `--json --no-log --top-k <n> <query>`
after the script path, exactly as it would for the real `ask.py`.

It never touches a real index, vault or session store: every branch emits a
synthetic `contract_version=1` envelope derived from its own location, so the
recall-side parsing, status and exit-code mapping can be exercised offline.

Branches:
  ok               one hit for notes/alpha.md with chunk_index/chunk_text
  ok-empty         a healthy envelope with zero results
  degraded         rerank unavailable: `degraded:true`, mode dense
  rerank-timeout   `detail.rerank.skipped = "timeout"`
  graph-fallback   `detail.graph_fallback = true`
  escalated        `detail.mode = "hybrid"`, `detail.escalated = ["hybrid","deep"]`
  chunk-missing    `chunk_index: null`, no `chunk_text`
  chunk-zero       `chunk_text` that does not occur in the note
  chunk-two        `chunk_text` that occurs twice in the note
  chunk-crlf-multiline  a chunk containing "\n" (cannot match a CRLF file)
  chunk-crlf-single     a single-line chunk (matches a CRLF file)
  dash-query       only succeeds when the query arrives as exactly `-foo`
  bm25-lexical     `--bm25` required; one all-term candidate plus one partial
                   candidate (caller must filter the partial row)
  bm25-empty       `--bm25` required; every candidate misses a term
  bm25-cjk         `--bm25` required; CJK whole-segment vs partial chunk
  bm25-dash        `--bm25` required; only succeeds for query `-foo`
  root-mismatch    a `vault_root` that differs from the configured root
  mode-missing     an envelope without `detail.mode`
  no-version       an envelope without `contract_version`
  bad-version      an envelope with `contract_version: 2`
  error            `LOCAL_UNAVAILABLE`, exit 12
  profile          `PROFILE_MISMATCH`, exit 13
  auth             `REMOTE_AUTH_REQUIRED`, exit 16
  unknown-exit     exit 14 with an `ACTIVATION_ABORTED` envelope
  exit2-empty      exit 2 with empty stdout (`--json` not recognised)
  plain-text       human text on stdout
  two-documents    two JSON documents on stdout
  timeout          prints nothing and sleeps (the caller kills it)
  writes-log       appends the B-level telemetry file, then a healthy hit
  writes-log-empty same write with zero results
  writes-cache     creates a C-level `__pycache__` artifact, then a healthy hit
"""

import json
import os
import sys
import time
from pathlib import Path

CONTRACT_VERSION = 1
ROOT = Path(__file__).resolve().parents[2]
NOTE = "notes/alpha.md"
CHUNK = "stub chunk"


def envelope(**overrides):
    payload = {
        "contract_version": CONTRACT_VERSION,
        "ok": True,
        "vault_root": str(ROOT),
        "degraded": False,
        "detail": {"mode": "dense", "escalated": []},
        "results": [],
    }
    payload.update(overrides)
    return payload


def hit(chunk_index=0, chunk_text=CHUNK, path=NOTE, score=0.75):
    row = {"path": path, "score": score, "chunk_index": chunk_index}
    if chunk_text is not None:
        row["chunk_text"] = chunk_text
    return row


def emit(payload, exit_code=0):
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    sys.stdout.write("\n")
    sys.stdout.flush()
    raise SystemExit(exit_code)


def bm25_row(path=NOTE, score=0.9, chunk_index=0, chunk_text=CHUNK):
    """One lexical candidate row, same shape the real `ask.py --bm25` returns."""
    return {
        "path": path,
        "score": score,
        "chunk_index": chunk_index,
        "chunk_text": chunk_text,
    }


def bm25_envelope(rows):
    payload = envelope(results=rows)
    payload["detail"] = {"mode": "bm25", "escalated": []}
    return payload


def require_bm25(bm25):
    """The lexical branches only answer `--bm25` calls: a missing flag makes
    the stub fail, which proves the caller did not append it."""
    if not bm25:
        emit(
            {
                "contract_version": CONTRACT_VERSION,
                "ok": False,
                "vault_root": str(ROOT),
                "error": {"code": "INVALID_REQUEST", "message": "stub requires --bm25"},
                "degraded": False,
                "detail": {"mode": "dense", "escalated": []},
                "results": [],
            },
            exit_code=2,
        )


def write_telemetry(name="usage-log.jsonl", body='{"event":"stub"}\n'):
    target = ROOT / ".meta" / name
    target.parent.mkdir(parents=True, exist_ok=True)
    with open(target, "a", encoding="utf-8") as handle:
        handle.write(body)


def main(argv):
    query = ""
    bm25 = False
    decay = None
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == "--":
            # argparse-style option terminator: the next token is the query
            # even when it starts with `-`.
            if index + 1 < len(argv):
                query = argv[index + 1]
            break
        if token == "--top-k" and index + 1 < len(argv):
            index += 2
            continue
        if token == "--decay" and index + 1 < len(argv):
            decay = argv[index + 1]
            index += 2
            continue
        if token == "--bm25":
            bm25 = True
        if not token.startswith("-"):
            query = token
        index += 1
    branch = os.environ.get("KB_STUB", "ok")

    if branch == "ok":
        emit(envelope(results=[hit()]))
    if branch == "ok-empty":
        emit(envelope())
    if branch == "decay-frozen":
        # Stage 7 §C: only the eval path appends the frozen decay triple. Any
        # other value (or a missing flag) fails the child, proving the caller.
        if decay != "1.0,1.0,1.0":
            emit(
                {
                    "contract_version": CONTRACT_VERSION,
                    "ok": False,
                    "vault_root": str(ROOT),
                    "error": {
                        "code": "INVALID_REQUEST",
                        "message": f"stub requires --decay 1.0,1.0,1.0, got {decay!r}",
                    },
                    "degraded": False,
                    "detail": {"mode": "dense", "escalated": []},
                    "results": [],
                },
                exit_code=2,
            )
        emit(envelope(results=[hit()]))
    if branch == "decay-absent":
        # The ordinary `memory_search` path must not send `--decay` at all.
        if decay is not None:
            emit(
                {
                    "contract_version": CONTRACT_VERSION,
                    "ok": False,
                    "vault_root": str(ROOT),
                    "error": {
                        "code": "INVALID_REQUEST",
                        "message": f"stub forbids --decay on this path, got {decay!r}",
                    },
                    "degraded": False,
                    "detail": {"mode": "dense", "escalated": []},
                    "results": [],
                },
                exit_code=2,
            )
        emit(envelope(results=[hit()]))
    if branch == "degraded":
        emit(envelope(degraded=True, results=[hit()]))
    if branch == "rerank-timeout":
        emit(envelope(
            detail={"mode": "dense", "escalated": [],
                    "rerank": {"skipped": "timeout", "elapsed_ms": 12}},
            results=[hit()],
        ))
    if branch == "graph-fallback":
        emit(envelope(detail={"mode": "dense", "escalated": [], "graph_fallback": True},
                      results=[hit()]))
    if branch == "escalated":
        emit(envelope(
            detail={"mode": "hybrid", "escalated": ["hybrid", "deep"]},
            results=[hit(), hit(path="notes/beta.md", score=0.5)],
        ))
    if branch == "chunk-missing":
        emit(envelope(results=[hit(chunk_index=None, chunk_text=None)]))
    if branch == "chunk-zero":
        emit(envelope(results=[hit(chunk_text="absent chunk text")]))
    if branch == "chunk-two":
        emit(envelope(results=[hit(chunk_text="dupchunk")]))
    if branch == "chunk-crlf-multiline":
        # The index stores LF-normalized text, so a chunk that contains a
        # newline can never be found in a CRLF file byte-for-byte.
        emit(envelope(results=[hit(chunk_text="line1\nneedle")]))
    if branch == "chunk-crlf-single":
        emit(envelope(results=[hit(chunk_text="needle")]))
    if branch == "dash-query":
        # Proves `--` reached the child: a legal `-foo` query must arrive intact.
        if query != "-foo":
            emit(
                {
                    "contract_version": CONTRACT_VERSION,
                    "ok": False,
                    "vault_root": str(ROOT),
                    "error": {"code": "INVALID_REQUEST", "message": f"unexpected query {query!r}"},
                    "degraded": False,
                    "detail": {"mode": "dense", "escalated": []},
                    "results": [],
                },
                exit_code=2,
            )
        emit(envelope(results=[hit()]))
    if branch == "bm25-lexical":
        # Mixed lexical candidates: `notes/alpha.md` carries every query term
        # ("AlphaBETA gamma" vs a lowercase query — case-folded match), while
        # the second row misses a term and must be filtered by the caller.
        require_bm25(bm25)
        emit(bm25_envelope([
            bm25_row(path="notes/alpha.md", score=0.9, chunk_text="AlphaBETA gamma"),
            bm25_row(path="notes/beta.md", score=0.6, chunk_text="only alpha"),
        ]))
    if branch == "bm25-empty":
        # Every candidate misses at least one query term: the caller filters
        # down to a healthy `empty` source.
        require_bm25(bm25)
        emit(bm25_envelope([
            bm25_row(path="notes/beta.md", score=0.6, chunk_text="only alpha"),
        ]))
    if branch == "bm25-cjk":
        # CJK whole-segment + bigram semantics: for query "统一回忆" the terms
        # are the whole segment plus adjacent bigrams, so only a chunk that
        # contains the full contiguous segment survives the whole-term filter.
        require_bm25(bm25)
        emit(bm25_envelope([
            bm25_row(path="notes/alpha.md", score=0.9, chunk_text="统一回忆"),
            bm25_row(path="notes/beta.md", score=0.6, chunk_text="统一"),
        ]))
    if branch == "bm25-dash":
        # `--bm25` must not disturb the `--` terminator: the dash query still
        # arrives as the positional argument, exactly as in the dense branch.
        # The chunk carries the query term so the caller's whole-term filter
        # keeps it.
        require_bm25(bm25)
        if query != "-foo":
            emit(
                {
                    "contract_version": CONTRACT_VERSION,
                    "ok": False,
                    "vault_root": str(ROOT),
                    "error": {"code": "INVALID_REQUEST", "message": f"unexpected query {query!r}"},
                    "degraded": False,
                    "detail": {"mode": "dense", "escalated": []},
                    "results": [],
                },
                exit_code=2,
            )
        emit(bm25_envelope([bm25_row(chunk_text="dash-foo-marker")]))
    if branch == "root-mismatch":
        emit(envelope(vault_root="/somewhere/else", results=[hit()]))
    if branch == "mode-missing":
        emit(envelope(detail={"escalated": []}, results=[hit()]))
    if branch == "no-version":
        payload = envelope(results=[hit()])
        payload.pop("contract_version")
        emit(payload)
    if branch == "bad-version":
        emit(envelope(contract_version=2, results=[hit()]))
    if branch == "error":
        emit(
            {
                "contract_version": CONTRACT_VERSION,
                "ok": False,
                "vault_root": str(ROOT),
                "error": {"code": "LOCAL_UNAVAILABLE", "message": "stub"},
                "degraded": False,
                "detail": {"mode": "dense", "escalated": []},
                "results": [],
            },
            exit_code=12,
        )
    if branch == "profile":
        emit(
            {
                "contract_version": CONTRACT_VERSION,
                "ok": False,
                "vault_root": str(ROOT),
                "error": {"code": "PROFILE_MISMATCH", "message": "stub"},
                "degraded": False,
                "detail": {"mode": "dense", "escalated": []},
                "results": [],
            },
            exit_code=13,
        )
    if branch == "auth":
        emit(
            {
                "contract_version": CONTRACT_VERSION,
                "ok": False,
                "vault_root": str(ROOT),
                "error": {"code": "REMOTE_AUTH_REQUIRED", "message": "stub"},
                "degraded": False,
                "detail": {"mode": "dense", "escalated": []},
                "results": [],
            },
            exit_code=16,
        )
    if branch == "unknown-exit":
        emit(
            {
                "contract_version": CONTRACT_VERSION,
                "ok": False,
                "vault_root": str(ROOT),
                "error": {"code": "ACTIVATION_ABORTED", "message": "stub"},
                "degraded": False,
                "detail": {"mode": "dense", "escalated": []},
                "results": [],
            },
            exit_code=14,
        )
    if branch == "exit2-empty":
        raise SystemExit(2)
    if branch == "plain-text":
        sys.stdout.write(f"查询: 「{query}」\n命中 0 条\n")
        sys.stdout.flush()
        raise SystemExit(0)
    if branch == "two-documents":
        sys.stdout.write(json.dumps(envelope()) + "\n")
        sys.stdout.write(json.dumps(envelope(results=[hit()])) + "\n")
        sys.stdout.flush()
        raise SystemExit(0)
    if branch == "timeout":
        time.sleep(60)
        emit(envelope())
    if branch == "writes-log":
        write_telemetry()
        emit(envelope(results=[hit()]))
    if branch == "writes-log-empty":
        write_telemetry()
        emit(envelope())
    if branch == "writes-cache":
        cache = ROOT / ".meta" / "__pycache__"
        cache.mkdir(parents=True, exist_ok=True)
        (cache / "stub.pyc").write_bytes(b"bytecode")
        emit(envelope(results=[hit()]))
    emit({"contract_version": CONTRACT_VERSION, "ok": False, "error": {"code": "BAD_STUB"}}, 2)


if __name__ == "__main__":
    main(sys.argv[1:])
