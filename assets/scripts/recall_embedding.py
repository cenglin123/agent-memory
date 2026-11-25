"""Offline embedding helper for Archive hybrid recall (P3).

Two entry points share one model identity:

* ``--probe`` freezes/report the capability facts (Python, model cache, backend)
  used by the degradation matrix. It never loads a model.
* ``--embed`` reads one JSON request from stdin and writes one JSON response to
  stdout::

      {"schema_version": 1, "texts": ["...", "..."],
       "model": {"model_id": "bge-small-zh-v1.5", "dimension": 512,
                 "normalization": "l2", "backend": "python",
                 "model_path": "<absolute cache or model dir>"},
       "ollama_url": "http://127.0.0.1:11434"}

  and answers ``{"ok": true, "vectors": [[...], ...], ...}``.

Backends:

* ``python`` — fastembed, loaded **offline only**: ``HF_HUB_OFFLINE=1`` is forced,
  and ``model_path`` must be an explicit existing directory. A missing cache
  degrades instead of downloading.
* ``ollama`` — loopback HTTP only (127.0.0.1 / localhost / ::1).
* ``fake`` — deterministic character-n-gram hashing, no model, no network. It is
  only reachable when the configuration explicitly says ``backend=fake`` and is
  reported as ``backend=fake``; it is never a fallback for a real backend.

Model unavailability is an explicit degradation: exit code 4 with an
``EMBEDDING_UNAVAILABLE`` envelope. The lexical recall path never depends on
this helper.
"""
import argparse
import json
import os
import platform
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

SCHEMA_VERSION = 1
MODEL_ID_PATTERN = re.compile(r'^[A-Za-z0-9._:/-]{1,128}$')
NORMALIZATIONS = ('l2', 'none')
BACKENDS = ('python', 'ollama', 'fake')
GENERATION_PREFIX = 'embed-v1'
EXIT_OK = 0
EXIT_USAGE = 2
EXIT_NOT_IMPLEMENTED = 3
EXIT_UNAVAILABLE = 4
MODEL_SCAN_MAX_ENTRIES = 16
MODEL_SCAN_MAX_DEPTH = 1
MODEL_SCAN_DEADLINE_SECONDS = 0.25
# Embedding is a batch model call, not a reachability probe: keep a generous
# default so a cold local model does not look unreachable.
DEFAULT_EMBED_TIMEOUT_SECONDS = 120.0

MAX_TEXTS = 4096
MAX_TEXT_BYTES = 65536
LOOPBACK_HOSTS = ('127.0.0.1', 'localhost', '::1')
FAKE_BACKEND = 'fake'


def generation_key(model_id, dimension, normalization, backend):
    return '%s;model=%s;dim=%d;norm=%s;backend=%s' % (
        GENERATION_PREFIX, model_id, dimension, normalization, backend)


def _validate(model_id, dimension, normalization, backend):
    if not isinstance(model_id, str) or not MODEL_ID_PATTERN.fullmatch(model_id):
        raise ValueError('model_id must be 1..=128 ASCII letters, digits or . _ : / -')
    if type(dimension) is not int or not 1 <= dimension <= 8192:
        raise ValueError('dimension must be an integer in 1..=8192')
    if normalization not in NORMALIZATIONS:
        raise ValueError('normalization must be l2 or none')
    if backend not in BACKENDS:
        raise ValueError('backend must be python, ollama or fake')


def _python_check():
    version = platform.python_version()
    available = sys.version_info >= (3, 9)
    return {
        'available': available,
        'version': version,
        'reason': None if available else 'python_version_below_3_9',
    }


def _model_check(model_path):
    if not model_path:
        return {
            'available': False,
            'path': None,
            'reason': 'model_path_not_configured',
            'files': 0,
            'scan_limited': False,
        }
    path = Path(model_path)
    if not path.is_dir():
        return {
            'available': False,
            'path': str(path),
            'reason': 'model_unavailable',
            'files': 0,
            'scan_limited': False,
        }

    # Existence is the only requirement. Counting is a bounded diagnostic:
    # top-level only, at most MODEL_SCAN_MAX_ENTRIES entries and a short
    # deadline, never a recursive tree walk.
    files = 0
    scanned = 0
    deadline = time.monotonic() + MODEL_SCAN_DEADLINE_SECONDS
    try:
        with os.scandir(path) as entries:
            for entry in entries:
                scanned += 1
                if scanned > MODEL_SCAN_MAX_ENTRIES or time.monotonic() > deadline:
                    break
                try:
                    if entry.is_file():
                        files += 1
                except OSError:
                    continue
    except OSError:
        files = 0
    return {
        'available': True,
        'path': str(path),
        'reason': None,
        'files': files,
        'scan_limited': scanned > MODEL_SCAN_MAX_ENTRIES,
        'max_entries': MODEL_SCAN_MAX_ENTRIES,
        'max_depth': MODEL_SCAN_MAX_DEPTH,
    }


def _ollama_check(url, timeout):
    if not url:
        return {
            'available': False,
            'url': None,
            'reason': 'ollama_url_not_configured',
        }
    endpoint = url.rstrip('/') + '/api/tags'
    try:
        with urllib.request.urlopen(endpoint, timeout=timeout) as response:
            available = 200 <= response.status < 300
        return {
            'available': available,
            'url': endpoint,
            'reason': None if available else 'ollama_unreachable',
        }
    except (urllib.error.URLError, OSError, ValueError) as error:
        return {
            'available': False,
            'url': endpoint,
            'reason': 'ollama_unreachable',
            'error': str(error),
        }


def probe(model_id=None, dimension=None, normalization='l2', backend='python',
          model_path=None, ollama_url=None, timeout=2.0):
    """Return a JSON-ready capability report. Never downloads and never raises
    for a missing optional component; invalid configuration raises ValueError.
    """
    if model_id is None or model_id == '':
        return {
            'ok': True,
            'schema_version': SCHEMA_VERSION,
            'status': 'disabled',
            'reason': 'embedding_not_configured',
            'lexical_continues': True,
            'generation': None,
            'model': None,
            'checks': {},
        }
    _validate(model_id, dimension, normalization, backend)

    python = _python_check()
    model = _model_check(model_path)
    if backend == 'ollama' and model['reason'] == 'model_path_not_configured':
        model['reason'] = 'not_required_for_ollama'
    if backend == FAKE_BACKEND:
        model['reason'] = 'not_required_for_fake'
        backend_check = {'available': python['available'],
                         'reason': None if python['available'] else 'python_unavailable'}
    elif backend == 'ollama':
        backend_check = _ollama_check(ollama_url, timeout)
    else:
        backend_check = {
            'available': model['available'],
            'reason': None if model['available'] else 'backend_unavailable',
        }

    if not python['available']:
        status, reason = 'degraded', 'python_unavailable'
    elif backend == FAKE_BACKEND:
        status, reason = 'available', None
    elif backend == 'ollama':
        # The model lives on the Ollama server: a local model_path is optional
        # and never gates this backend. Missing/unreachable URL is one reason.
        if not backend_check['available']:
            status, reason = 'degraded', 'ollama_unreachable'
        else:
            status, reason = 'available', None
    elif not model['available']:
        status, reason = 'degraded', 'model_unavailable'
    elif not backend_check['available']:
        status, reason = 'degraded', 'backend_unavailable'
    else:
        status, reason = 'available', None

    return {
        'ok': True,
        'schema_version': SCHEMA_VERSION,
        'status': status,
        'reason': reason,
        'lexical_continues': True,
        'generation': generation_key(model_id, dimension, normalization, backend),
        'model': {
            'model_id': model_id,
            'dimension': dimension,
            'normalization': normalization,
            'backend': backend,
            'model_path': model_path,
        },
        'checks': {
            'python': python,
            'model': model,
            'backend': backend_check,
        },
    }


# ---------------------------------------------------------------------------
# Fake backend: deterministic, model-free, test-only
# ---------------------------------------------------------------------------

_FNV_OFFSET = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_U64 = 0xFFFFFFFFFFFFFFFF


def _fnv1a64(data):
    value = _FNV_OFFSET
    for byte in data:
        value ^= byte
        value = (value * _FNV_PRIME) & _U64
    return value


def fake_vector(text, dimension):
    """Deterministic character-n-gram hashing (byte trigrams + unigrams).

    Mirrored bit-for-bit by ``semantic::embed::fake_vector`` in Rust so the fake
    backend has one definition. Not a semantic model: it only makes the
    hybrid pipeline observable in tests without any model file.
    """
    data = text.encode('utf-8', 'surrogatepass')
    vector = [0.0] * dimension
    if not data:
        return vector
    windows = [data]
    if len(data) > 3:
        windows = [data[i:i + 3] for i in range(len(data) - 2)]
    for gram in windows:
        hashed = _fnv1a64(gram)
        index = hashed % dimension
        sign = -1.0 if (hashed >> 63) & 1 else 1.0
        vector[index] += sign
    return vector


def _l2(vector):
    norm = sum(value * value for value in vector) ** 0.5
    if norm == 0.0:
        return vector
    return [value / norm for value in vector]


# ---------------------------------------------------------------------------
# Real backends
# ---------------------------------------------------------------------------

class EmbeddingUnavailable(Exception):
    def __init__(self, reason, detail=None):
        super().__init__(reason)
        self.reason = reason
        self.detail = detail or {}


def _offline_env():
    """Force an offline Hugging Face/transformers resolution.

    The helper must never download a model during install or replay; a missing
    cache is a degradation, not a prompt to fetch weights.
    """
    env = dict(os.environ)
    env['HF_HUB_OFFLINE'] = '1'
    env['TRANSFORMERS_OFFLINE'] = '1'
    env['HF_HUB_DISABLE_TELEMETRY'] = '1'
    return env


def _fastembed_cache_dir(model_path):
    path = Path(model_path)
    if path.name.lower().startswith('models--'):
        return str(path.parent)
    return str(path)


def _resolve_fastembed_model(model_id, TextEmbedding):
    supported = TextEmbedding.list_supported_models()
    names = [entry.get('model', '') for entry in supported]
    for name in names:
        if name == model_id:
            return name
    lowered = model_id.lower()
    matches = [name for name in names if lowered in name.lower()]
    if len(matches) == 1:
        return matches[0]
    if matches:
        return sorted(matches)[0]
    raise EmbeddingUnavailable('model_not_supported_by_fastembed',
                               {'model_id': model_id})


def _python_embed(model, texts):
    model_path = model.get('model_path')
    if not model_path:
        raise EmbeddingUnavailable('model_path_not_configured')
    if not Path(model_path).is_dir():
        raise EmbeddingUnavailable('model_unavailable', {'model_path': model_path})
    for key, value in _offline_env().items():
        if key in ('HF_HUB_OFFLINE', 'TRANSFORMERS_OFFLINE', 'HF_HUB_DISABLE_TELEMETRY'):
            os.environ[key] = value
    try:
        from fastembed import TextEmbedding
    except ImportError as error:
        raise EmbeddingUnavailable('backend_unavailable', {'error': str(error)})
    name = _resolve_fastembed_model(model['model_id'], TextEmbedding)
    try:
        embedder = TextEmbedding(model_name=name,
                                 cache_dir=_fastembed_cache_dir(model_path))
        vectors = [list(map(float, vector)) for vector in embedder.embed(texts)]
    except Exception as error:  # fastembed raises several unrelated types
        raise EmbeddingUnavailable('backend_unavailable', {'error': str(error)})
    return vectors, name


def _loopback(url):
    parsed = urllib.parse.urlparse(url)
    return parsed.scheme in ('http', 'https') and parsed.hostname in LOOPBACK_HOSTS


def _ollama_embed(url, model_id, texts, timeout):
    if not url or not _loopback(url):
        raise EmbeddingUnavailable('ollama_url_not_loopback', {'url': url})
    endpoint = url.rstrip('/') + '/api/embed'
    payload = json.dumps({'model': model_id, 'input': texts}).encode('utf-8')
    request = urllib.request.Request(endpoint, data=payload,
                                     headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = json.loads(response.read().decode('utf-8'))
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise EmbeddingUnavailable('ollama_unreachable', {'error': str(error)})
    vectors = body.get('embeddings')
    if not isinstance(vectors, list) or len(vectors) != len(texts):
        raise EmbeddingUnavailable('ollama_invalid_response')
    return [list(map(float, vector)) for vector in vectors], model_id


def _finalize(vectors, dimension, normalization):
    """Truncate to the configured dimension (recorded in the generation key) and
    apply normalization. A model narrower than the configuration is an error.
    """
    out = []
    native = None
    for vector in vectors:
        if native is None:
            native = len(vector)
        if len(vector) != native:
            raise EmbeddingUnavailable('dimension_mismatch')
        if native < dimension:
            raise EmbeddingUnavailable('dimension_mismatch',
                                       {'native': native, 'configured': dimension})
        trimmed = vector[:dimension]
        out.append(_l2(trimmed) if normalization == 'l2' else trimmed)
    return out, native


def embed_request(request, timeout=DEFAULT_EMBED_TIMEOUT_SECONDS):
    """Embed a batch from a parsed request. Raises ValueError for invalid input
    and EmbeddingUnavailable for a missing capability.
    """
    if not isinstance(request, dict):
        raise ValueError('request must be a JSON object')
    texts = request.get('texts')
    if not isinstance(texts, list) or not texts:
        raise ValueError('texts must be a non-empty array')
    if len(texts) > MAX_TEXTS:
        raise ValueError('texts must contain at most %d entries' % MAX_TEXTS)
    if any(not isinstance(text, str) for text in texts):
        raise ValueError('every text must be a string')
    if any(len(text.encode('utf-8', 'surrogatepass')) > MAX_TEXT_BYTES for text in texts):
        raise ValueError('every text must be at most %d bytes' % MAX_TEXT_BYTES)

    model = request.get('model')
    if not isinstance(model, dict):
        raise ValueError('model must be an object')
    model_id = model.get('model_id')
    dimension = model.get('dimension')
    normalization = model.get('normalization', 'l2')
    backend = model.get('backend', 'python')
    model_path = model.get('model_path')
    _validate(model_id, dimension, normalization, backend)

    if backend == FAKE_BACKEND:
        vectors = [fake_vector(text, dimension) for text in texts]
        if normalization == 'l2':
            vectors = [_l2(vector) for vector in vectors]
        return {
            'ok': True,
            'schema_version': SCHEMA_VERSION,
            'status': 'available',
            'backend': FAKE_BACKEND,
            'model_id': model_id,
            'dimension': dimension,
            'normalization': normalization,
            'generation': generation_key(model_id, dimension, normalization, backend),
            'native_dimension': dimension,
            'encoding': 'f32',
            'vectors': vectors,
        }

    if backend == 'ollama':
        url = request.get('ollama_url') or 'http://127.0.0.1:11434'
        vectors, resolved = _ollama_embed(url, model_id, texts, timeout)
    else:
        vectors, resolved = _python_embed(model, texts)
    final, native = _finalize(vectors, dimension, normalization)
    return {
        'ok': True,
        'schema_version': SCHEMA_VERSION,
        'status': 'available',
        'backend': backend,
        'model_id': resolved,
        'dimension': dimension,
        'normalization': normalization,
        'generation': generation_key(model_id, dimension, normalization, backend),
        'native_dimension': native,
        'encoding': 'f32',
        'vectors': final,
    }


def _unavailable(reason, detail=None):
    payload = {
        'ok': False,
        'schema_version': SCHEMA_VERSION,
        'status': 'degraded',
        'code': 'EMBEDDING_UNAVAILABLE',
        'reason': reason,
        'lexical_continues': True,
    }
    if detail:
        payload['detail'] = detail
    return payload


def main(argv=None):
    parser = argparse.ArgumentParser()
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument('--probe', action='store_true')
    action.add_argument('--embed', action='store_true')
    parser.add_argument('--model-id')
    parser.add_argument('--dimension', type=int)
    parser.add_argument('--normalization', default='l2')
    parser.add_argument('--backend', default='python')
    parser.add_argument('--model-path')
    parser.add_argument('--ollama-url')
    parser.add_argument('--timeout', type=float, default=2.0)
    parser.add_argument('--embed-timeout', type=float, default=DEFAULT_EMBED_TIMEOUT_SECONDS)
    args = parser.parse_args(argv)

    if args.embed:
        raw = sys.stdin.read()
        try:
            request = json.loads(raw)
        except ValueError as error:
            print(json.dumps({
                'ok': False,
                'schema_version': SCHEMA_VERSION,
                'code': 'INVALID_REQUEST',
                'reason': 'request is not valid JSON: %s' % error,
                'lexical_continues': True,
            }, ensure_ascii=False))
            return EXIT_USAGE
        if not request.get('model') and args.model_id:
            request['model'] = {
                'model_id': args.model_id,
                'dimension': args.dimension,
                'normalization': args.normalization,
                'backend': args.backend,
                'model_path': args.model_path,
            }
            if args.ollama_url:
                request['ollama_url'] = args.ollama_url
        try:
            report = embed_request(request, args.embed_timeout)
        except ValueError as error:
            print(json.dumps({
                'ok': False,
                'schema_version': SCHEMA_VERSION,
                'code': 'INVALID_REQUEST',
                'reason': str(error),
                'lexical_continues': True,
            }, ensure_ascii=False))
            return EXIT_USAGE
        except EmbeddingUnavailable as error:
            print(json.dumps(_unavailable(error.reason, error.detail),
                             ensure_ascii=False))
            return EXIT_UNAVAILABLE
        print(json.dumps(report, ensure_ascii=False))
        return EXIT_OK

    try:
        report = probe(args.model_id, args.dimension, args.normalization,
                       args.backend, args.model_path, args.ollama_url, args.timeout)
    except ValueError as error:
        print(json.dumps({
            'ok': False,
            'schema_version': SCHEMA_VERSION,
            'code': 'INVALID_REQUEST',
            'reason': str(error),
            'lexical_continues': True,
        }, ensure_ascii=False))
        return EXIT_USAGE
    print(json.dumps(report, ensure_ascii=False))
    return EXIT_OK


if __name__ == '__main__':
    sys.exit(main())
