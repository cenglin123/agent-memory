"""Distribute the agent-memory skill to the local hosts.

Standard library only. Owned by 64-install-recall.ps1 and 90-audit.ps1:

* ``onboard`` writes the copy (cc-switch first, standalone fallback),
* ``check`` is a read-only report used by ``64 -CheckOnly``,
* ``audit`` returns the ``AM-SKILL`` grading used by 90,
* ``remove`` deletes only copies recorded in the deploy marker.

No discovery of roots beyond the explicit ``user_root`` / ``local_appdata``.
cc-switch is only used when its database exists *and* its ``skills`` table
carries every column of the known schema; a partial table fails closed before
anything is written.
"""
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from contextlib import closing

SKILL_NAME = 'agent-memory'
SKILL_ID = 'local:' + SKILL_NAME
MARKER_SCHEMA = 1
DEFAULT_SOURCE = Path(__file__).resolve().parent.parent / 'skills' / SKILL_NAME

# The full column set of the cc-switch schema this module was written against.
# A missing column means the table belongs to another version: fail closed.
CC_REQUIRED_COLUMNS = (
    'id', 'name', 'description', 'directory', 'repo_owner', 'repo_name',
    'repo_branch', 'readme_url', 'enabled_claude', 'enabled_codex',
    'enabled_gemini', 'enabled_opencode', 'installed_at', 'content_hash',
    'updated_at', 'enabled_hermes', 'enabled_grokbuild', 'enabled_mcode',
    'enabled_agy', 'enabled_kimi', 'enabled_dsh',
)

# Hosts whose skill directory has been verified on this machine.
CC_HOSTS = (
    ('claude', '.claude'),
    ('codex', '.codex'),
    ('opencode', '.config/opencode'),
    # DSH reads skills from <UserRoot>/.dsh/skills (verified on the reference
    # machine); the root is only used when <UserRoot>/.dsh already exists.
    ('dsh', '.dsh'),
)
# Hermes lives under LocalAppData, not under the user root.
UNVERIFIED_HOSTS = ('gemini', 'grokbuild', 'mcode', 'agy', 'kimi')
DEEPSEEK_REL = '.deepseek/skills'


class SkillError(ValueError):
    """A blocking configuration problem; nothing has been written."""


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

def now_iso():
    return datetime.datetime.now().astimezone().isoformat(timespec='seconds')


def _text(path):
    with open(path, 'r', encoding='utf-8-sig', newline='') as handle:
        return handle.read()


def _write_json(path, payload):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    data = (json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + '\n').encode('utf-8')
    handle, temp = tempfile.mkstemp(prefix=path.name + '.', suffix='.suite-tmp', dir=str(path.parent))
    try:
        with os.fdopen(handle, 'wb') as out:
            out.write(data)
        os.replace(temp, path)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)


def _inside(child, root):
    child = os.path.abspath(str(child)).replace('\\', '/').rstrip('/').lower()
    root = os.path.abspath(str(root)).replace('\\', '/').rstrip('/').lower()
    return child == root or child.startswith(root + '/')


def _unique_dir(parent):
    parent = Path(parent)
    parent.parent.mkdir(parents=True, exist_ok=True)
    candidate = parent
    index = 1
    while candidate.exists():
        candidate = parent.with_name(parent.name + '-' + str(index))
        index += 1
    candidate.mkdir(parents=True)
    return candidate


def _item(name, status, detail):
    return dict(item=name, status=status, detail=detail)


def _short_hash(value):
    return (value or '')[:12]


# ---------------------------------------------------------------------------
# Source, frontmatter and content hash
# ---------------------------------------------------------------------------

def content_hash(directory):
    """cc-switch local-source hash: path + NUL + bytes + NUL, sorted by path.

    Hidden path segments (any segment starting with ``.``) are skipped.
    """
    root = Path(directory)
    entries = []
    for path in root.rglob('*'):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        if any(part.startswith('.') for part in relative.split('/')):
            continue
        entries.append((relative, path))
    entries.sort(key=lambda item: item[0])
    digest = hashlib.sha256()
    for relative, path in entries:
        digest.update(relative.encode('utf-8'))
        digest.update(b'\0')
        digest.update(path.read_bytes())
        digest.update(b'\0')
    return digest.hexdigest()


def parse_frontmatter(skill_md):
    """Return (name, description) from a minimal ``---`` YAML block."""
    lines = _text(skill_md).splitlines()
    if not lines or lines[0].strip() != '---':
        return SKILL_NAME, None
    values = {}
    for line in lines[1:]:
        if line.strip() == '---':
            break
        match = re.match(r'^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*(.*)$', line)
        if not match:
            continue
        key, value = match.group(1), match.group(2).strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
            value = value[1:-1]
        values[key] = value
    name = values.get('name') or SKILL_NAME
    if name != SKILL_NAME:
        raise SkillError('技能 frontmatter name 必须是 %s，实际是 %s' % (SKILL_NAME, name))
    return name, values.get('description') or None


def source_state(source):
    source = Path(source)
    if not source.is_dir():
        raise SkillError('技能来源目录不存在：%s' % source)
    skill_md = source / 'SKILL.md'
    if not skill_md.is_file():
        raise SkillError('技能来源缺少 SKILL.md：%s' % source)
    name, description = parse_frontmatter(skill_md)
    return dict(path=source, hash=content_hash(source), name=name,
                description=description)


# ---------------------------------------------------------------------------
# cc-switch
# ---------------------------------------------------------------------------

def cc_db_path(user_root):
    return Path(user_root) / '.cc-switch' / 'cc-switch.db'


def cc_snapshot(user_root):
    """Return None when cc-switch is absent; raise when its schema is partial."""
    db = cc_db_path(user_root)
    if not db.is_file():
        return None
    try:
        connection = sqlite3.connect(db.as_uri() + '?mode=ro', uri=True)
    except sqlite3.Error as err:
        raise SkillError('无法打开 cc-switch 数据库：%s' % err)
    with closing(connection) as conn:
        table = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='skills'").fetchone()
        if not table:
            raise SkillError('cc-switch 数据库缺少 skills 表；拒绝猜测 schema（fail closed，未写入）')
        columns = [row[1] for row in conn.execute('PRAGMA table_info(skills)')]
        missing = [name for name in CC_REQUIRED_COLUMNS if name not in columns]
        if missing:
            raise SkillError('cc-switch skills 表缺列（%s）；fail closed，未写入任何内容'
                             % ', '.join(missing))
        row = conn.execute('SELECT * FROM skills WHERE id=?', (SKILL_ID,)).fetchone()
        record = None
        if row is not None:
            record = dict(zip(columns, row))
    return dict(db=db, columns=columns, row=record)


def _flags(row):
    """Row flags; a fresh row enables the five known skill hosts."""
    enabled = {}
    for host in ('claude', 'codex', 'opencode', 'hermes', 'dsh'):
        enabled[host] = 1 if row is None else int(row.get('enabled_' + host) or 0)
    for host in UNVERIFIED_HOSTS:
        enabled[host] = 0 if row is None else int(row.get('enabled_' + host) or 0)
    return enabled


def _backup_db(user_root):
    db = cc_db_path(user_root)
    base = Path(user_root) / '.cc-switch' / 'backups' / ('skill-' + time.strftime('%Y%m%d-%H%M%S'))
    target_dir = _unique_dir(base)
    backup = target_dir / 'cc-switch.db'
    with closing(sqlite3.connect(db.as_uri() + '?mode=ro', uri=True)) as source:
        with closing(sqlite3.connect(str(backup))) as destination:
            source.backup(destination)
    return backup


def _backup_ssot(user_root):
    ssot = Path(user_root) / '.cc-switch' / 'skills' / SKILL_NAME
    base = Path(user_root) / '.cc-switch' / 'skill-backups' / (
        time.strftime('%Y%m%d_%H%M%S') + '_' + SKILL_NAME)
    target_dir = _unique_dir(base)
    shutil.copytree(str(ssot), str(target_dir / 'skill'))
    meta = dict(schema=MARKER_SCHEMA, name=SKILL_NAME, directory=SKILL_NAME,
                source_hash=content_hash(ssot), backed_up_at=now_iso(), reason='update')
    _write_json(target_dir / 'meta.json', meta)
    return target_dir


def _upsert_row(user_root, snapshot, source):
    """Insert or update the local source row; returns (status, detail, row)."""
    row = snapshot['row']
    desired = dict(name=source['name'], description=source['description'],
                   directory=SKILL_NAME, content_hash=source['hash'])
    if row is not None:
        changes = {key: value for key, value in desired.items() if row.get(key) != value}
        if not changes:
            return 'unchanged', 'cc-switch 行已存在且哈希一致；保留既有 enabled_* 开关', row
        backup = _backup_db(user_root)
        with closing(sqlite3.connect(str(snapshot['db']))) as conn:
            with conn:
                conn.execute(
                    'UPDATE skills SET name=?, description=?, directory=?, '
                    'content_hash=?, updated_at=? WHERE id=?',
                    (desired['name'], desired['description'], desired['directory'],
                     desired['content_hash'], int(time.time()), SKILL_ID))
        return 'updated', 'cc-switch 行已更新（保留用户开关）；备份 ' + str(backup), row
    backup = _backup_db(user_root)
    timestamp = int(time.time())
    values = dict(id=SKILL_ID, name=desired['name'], description=desired['description'],
                  directory=SKILL_NAME, repo_owner=None, repo_name=None,
                  repo_branch=None, readme_url=None, enabled_claude=1,
                  enabled_codex=1, enabled_gemini=0, enabled_opencode=1,
                  installed_at=timestamp, content_hash=source['hash'],
                  updated_at=timestamp, enabled_hermes=1, enabled_grokbuild=0,
                  enabled_mcode=0, enabled_agy=0, enabled_kimi=0, enabled_dsh=1)
    columns = [name for name in CC_REQUIRED_COLUMNS]
    with closing(sqlite3.connect(str(snapshot['db']))) as conn:
        with conn:
            conn.execute(
                'INSERT INTO skills (%s) VALUES (%s)'
                % (', '.join(columns), ', '.join('?' for _ in columns)),
                [values[name] for name in columns])
    return 'created', 'cc-switch 新登记 %s（claude/codex/opencode/hermes/dsh=1）；备份 %s' \
        % (SKILL_ID, backup), None


# ---------------------------------------------------------------------------
# Targets
# ---------------------------------------------------------------------------

def host_roots(user_root, local_appdata):
    roots = {}
    for host, rel in CC_HOSTS:
        roots[host] = Path(user_root) / Path(rel)
    if local_appdata:
        roots['hermes'] = Path(local_appdata) / 'hermes'
    return roots


def skill_dir(host_root):
    """``<host root>/skills/agent-memory`` guarded against traversal."""
    host_root = Path(host_root)
    target = Path(os.path.abspath(str(host_root / 'skills' / SKILL_NAME)))
    if target.name != SKILL_NAME or not _inside(target, host_root):
        raise SkillError('拒绝越界的技能目标：%s（宿主根 %s）' % (target, host_root))
    return target


def deepseek_dir(user_root):
    return Path(user_root) / DEEPSEEK_REL / SKILL_NAME


def _copy_tree(source, target):
    """Stage in a sibling temp directory, then replace target.

    On Windows ``tempfile.mkdtemp`` creates a protected DACL (mode 0o700); an
    elevated 64 would then deploy a copy the normal-token audit cannot read.
    A plain ``mkdir`` under ``target.parent`` inherits that parent's ACL. On
    POSIX the old 0700 staging mode is kept.
    """
    source = Path(source)
    target = Path(os.path.abspath(str(target)))
    target.parent.mkdir(parents=True, exist_ok=True)
    temp = target.parent / ('.' + SKILL_NAME + '.tmp-' + uuid.uuid4().hex)
    if os.name == 'nt':
        temp.mkdir()
    else:
        temp.mkdir(mode=0o700)
    staged = temp / SKILL_NAME
    stale = None
    completed = False
    try:
        shutil.copytree(str(source), str(staged))
        if target.exists():
            stale = target.parent / ('.' + SKILL_NAME + '.old-' + uuid.uuid4().hex)
            os.replace(str(target), str(stale))
        os.replace(str(staged), str(target))
        completed = True
    finally:
        if stale is not None:
            if completed:
                shutil.rmtree(str(stale), ignore_errors=True)
            elif not target.exists():
                # The staged -> target replacement failed: put the original
                # back. If that also fails, leave the stale copy on disk; never
                # delete the only remaining original.
                try:
                    os.replace(str(stale), str(target))
                except OSError:
                    pass
        shutil.rmtree(str(temp), ignore_errors=True)


def deploy_copy(source, target, source_hash):
    """Deploy without touching an identical copy; returns (status, hash)."""
    target = Path(target)
    if target.is_dir():
        current = content_hash(target)
        if current == source_hash:
            return 'unchanged', current
        _copy_tree(source, target)
        return 'updated', source_hash
    _copy_tree(source, target)
    return 'created', source_hash


def managed_roots(user_root, local_appdata=None):
    roots = [Path(user_root) / '.agents', Path(user_root) / '.deepseek']
    for _, rel in CC_HOSTS:
        roots.append(Path(user_root) / Path(rel))
    if local_appdata:
        roots.append(Path(local_appdata) / 'hermes')
    return roots


def is_managed_target(path, user_root, local_appdata=None):
    target = Path(path)
    if target.name != SKILL_NAME:
        return False
    return any(_inside(target, root) for root in managed_roots(user_root, local_appdata))


def remove_copy(target):
    target = Path(target)
    if target.name != SKILL_NAME:
        raise SkillError('拒绝删除非 %s 目录：%s' % (SKILL_NAME, target))
    if target.exists():
        shutil.rmtree(str(target))


# ---------------------------------------------------------------------------
# Deploy marker
# ---------------------------------------------------------------------------

def marker_path(user_root):
    return Path(user_root) / '.agent-memory' / 'skill-deploy.json'


def read_marker(user_root):
    path = marker_path(user_root)
    if not path.is_file():
        return None, None
    try:
        payload = json.loads(_text(path))
    except (OSError, ValueError) as err:
        return None, str(err)
    if not isinstance(payload, dict):
        return None, '标记不是 JSON 对象'
    return payload, None


# ---------------------------------------------------------------------------
# Onboarding
# ---------------------------------------------------------------------------

def preflight(source, user_root, local_appdata=None):
    """Validate everything that could block a write; raises before writing."""
    state = source_state(source)
    snapshot = cc_snapshot(user_root)
    return dict(source=state, cc=snapshot,
                roots=host_roots(user_root, local_appdata))


def _source_item(source):
    return _item('skill-source', 'PASS',
                 '%s；哈希 %s' % (source['path'], source['hash']))


def _unsupported_items(enabled):
    items = []
    for host in UNVERIFIED_HOSTS:
        if enabled.get(host):
            items.append(_item('skill-unsupported', 'SKIP',
                               '%s 已在 cc-switch 启用，但本机部署目录未核实；'
                               '请在 cc-switch 界面完成部署（不猜测路径）' % host))
    return items


def _onboard_cc(ctx, user_root):
    source = ctx['source']
    snapshot = ctx['cc']
    items = []
    targets = []

    ssot = Path(user_root) / '.cc-switch' / 'skills' / SKILL_NAME
    if ssot.is_dir() and content_hash(ssot) == source['hash']:
        items.append(_item('skill-ssot', 'PASS',
                           'SSOT 已是最新，未改写：%s' % ssot))
    else:
        backup = None
        if ssot.is_dir():
            backup = _backup_ssot(user_root)
        _copy_tree(source['path'], ssot)
        items.append(_item('skill-ssot', 'PASS',
                           'SSOT 已同步：%s%s'
                           % (ssot, '' if backup is None else '；备份 ' + str(backup))))

    _, detail, _ = _upsert_row(user_root, snapshot, source)
    items.append(_item('skill-cc-switch', 'PASS', detail))

    row = snapshot['row']
    enabled = _flags(row)
    for host, root in ctx['roots'].items():
        if not root.is_dir():
            items.append(_item('skill-target:' + host, 'SKIP',
                               '宿主根不存在，跳过（不创建）：%s' % root))
            continue
        target = skill_dir(root)
        if enabled.get(host):
            state, digest = deploy_copy(source['path'], target, source['hash'])
            items.append(_item('skill-target:' + host, 'PASS',
                               '%s：%s（%s）' % (state, target, _short_hash(digest))))
            targets.append(dict(host=host, path=str(target), hash=digest))
        elif target.is_dir():
            if content_hash(target) == source['hash']:
                remove_copy(target)
                items.append(_item('skill-target:' + host, 'PASS',
                                   '宿主已关闭；已删除与来源一致的副本：%s' % target))
            else:
                items.append(_item('skill-target:' + host, 'WARN',
                                   '宿主已关闭，但副本内容与来源不一致，保留待人工确认：%s' % target))
        else:
            items.append(_item('skill-target:' + host, 'PASS',
                               '宿主已关闭且无副本：%s' % target))

    items.extend(_unsupported_items(enabled))
    return items, targets


def _onboard_standalone(ctx, user_root):
    source = ctx['source']
    items = [_item('skill-cc-switch', 'SKIP',
                   'cc-switch 不在位；使用 standalone 部署（无 DB 写入）')]
    targets = []
    agents_root = Path(user_root) / '.agents'
    agents_target = skill_dir(agents_root)
    state, digest = deploy_copy(source['path'], agents_target, source['hash'])
    items.append(_item('skill-target:agents', 'PASS',
                       '%s：%s（%s）' % (state, agents_target, _short_hash(digest))))
    targets.append(dict(host='agents', path=str(agents_target), hash=digest))
    for host, root in ctx['roots'].items():
        if not root.is_dir():
            items.append(_item('skill-target:' + host, 'SKIP',
                               '宿主根不存在，跳过（不创建）：%s' % root))
            continue
        target = skill_dir(root)
        state, digest = deploy_copy(source['path'], target, source['hash'])
        items.append(_item('skill-target:' + host, 'PASS',
                           '%s：%s（%s）' % (state, target, _short_hash(digest))))
        targets.append(dict(host=host, path=str(target), hash=digest))
    return items, targets


def _deploy_deepseek(ctx, user_root, targets, items):
    source = ctx['source']
    root = Path(user_root) / '.deepseek'
    skills = root / 'skills'
    if not skills.is_dir():
        items.append(_item('skill-target:deepseek', 'SKIP',
                           'DeepSeek CLI 技能目录不存在，跳过：%s' % skills))
        return
    target = skill_dir(root)
    state, digest = deploy_copy(source['path'], target, source['hash'])
    items.append(_item('skill-target:deepseek', 'PASS',
                       '%s：%s（%s）' % (state, target, _short_hash(digest))))
    targets.append(dict(host='deepseek', path=str(target), hash=digest))


def onboard(source, user_root, local_appdata=None, register_mcp=False):
    """Deploy the skill; returns report items."""
    ctx = preflight(source, user_root, local_appdata)
    source_state_ = ctx['source']
    items = [_source_item(source_state_)]
    if ctx['cc'] is not None:
        mode = 'cc-switch'
        deployed, targets = _onboard_cc(ctx, user_root)
    else:
        mode = 'standalone'
        deployed, targets = _onboard_standalone(ctx, user_root)
    items.extend(deployed)
    _deploy_deepseek(ctx, user_root, targets, items)
    previous, _ = read_marker(user_root)
    marker = dict(schema=MARKER_SCHEMA,
                  installed_at=(previous or {}).get('installed_at') or now_iso(),
                  source_hash=source_state_['hash'], mode=mode, targets=targets)
    if previous == marker:
        items.append(_item('skill-marker', 'PASS',
                           '标记已是最新，未改写：%s' % marker_path(user_root)))
    else:
        _write_json(marker_path(user_root), marker)
        items.append(_item('skill-marker', 'PASS',
                           '已写入 %s（mode=%s，%d 个目标）'
                           % (marker_path(user_root), mode, len(targets))))
    items.extend(mcp_items(user_root, cc_mode=(mode == 'cc-switch')))
    if register_mcp:
        items.extend(mcp_register(user_root, cc_mode=(mode == 'cc-switch')))
    return items


# ---------------------------------------------------------------------------
# Read-only checks (64 -CheckOnly)
# ---------------------------------------------------------------------------

def _target_item(name, source_hash, target, required=True):
    target = Path(target)
    if not target.is_dir():
        status = 'FAIL' if required else 'WARN'
        detail = '副本缺失：%s' % target if required else '未部署：%s' % target
        return _item(name, status, detail)
    current = content_hash(target)
    if current != source_hash:
        status = 'FAIL' if required else 'WARN'
        return _item(name, status, '副本哈希与来源不一致：%s（%s）'
                                  % (target, _short_hash(current)))
    return _item(name, 'PASS', '哈希一致：%s' % target)


def _normalized(path):
    return str(path).replace('\\', '/').lower()


def _marker_target_paths(marker):
    paths = set()
    if not isinstance(marker, dict):
        return paths
    for entry in marker.get('targets') or []:
        if isinstance(entry, dict) and isinstance(entry.get('path'), str):
            paths.add(_normalized(entry['path']))
    return paths


def _cc_rows(enabled, row, source_hash, ssot, required):
    items = []
    if row is None:
        items.append(_item('skill-cc-switch', 'FAIL' if required else 'WARN',
                           'cc-switch 缺少 %s 行；%s'
                           % (SKILL_ID, '重跑 64 登记' if required
                              else '尚未登记（重新运行 64）')))
        if ssot.is_dir():
            matched = content_hash(ssot) == source_hash
            items.append(_item('skill-ssot',
                               'PASS' if matched else ('FAIL' if required else 'WARN'),
                               ('SSOT 与来源一致：' if matched else 'SSOT 与来源不一致：')
                               + str(ssot)))
        return items, False
    shown = ','.join('%s=%d' % (host, enabled.get(host, 0))
                     for host in ('claude', 'codex', 'opencode', 'hermes', 'dsh'))
    if row.get('content_hash') != source_hash:
        items.append(_item('skill-cc-switch', 'FAIL',
                           'cc-switch 行 content_hash 与来源不一致（%s / %s）；%s'
                           % (_short_hash(row.get('content_hash')),
                              _short_hash(source_hash), shown)))
        ssot_ok = ssot.is_dir() and content_hash(ssot) == source_hash
        if not ssot_ok:
            items.append(_item('skill-ssot', 'FAIL',
                               'SSOT 缺失或与来源不一致：%s' % ssot))
        return items, False
    ssot_ok = ssot.is_dir() and content_hash(ssot) == source_hash
    if not ssot_ok:
        items.append(_item('skill-ssot', 'FAIL',
                           'SSOT 缺失或与来源不一致：%s' % ssot))
    else:
        items.append(_item('skill-ssot', 'PASS', 'SSOT 与来源一致：%s' % ssot))
    items.append(_item('skill-cc-switch', 'PASS',
                       '行 %s 哈希一致；%s' % (SKILL_ID, shown)))
    return items, ssot_ok


def check(source, user_root, local_appdata=None):
    """Read-only report; never writes."""
    items = []
    marker, marker_error = read_marker(user_root)
    try:
        ctx = preflight(source, user_root, local_appdata)
    except SkillError as err:
        return [_item('skill-source', 'FAIL', str(err)),
                _item('skill-marker', 'WARN',
                      '无部署标记' if marker is None and not marker_error
                      else '部署标记不可用：%s' % (marker_error or '')),
                _item('am-skill', 'FAIL', '技能分发检查失败：%s' % err)]
    source_state_ = ctx['source']
    items.append(_source_item(source_state_))
    has_marker = marker is not None and marker_error is None
    marker_paths = _marker_target_paths(marker) if has_marker else set()
    row = ctx['cc']['row'] if ctx['cc'] is not None else None
    required = has_marker or row is not None
    cc_row_exists = False
    if ctx['cc'] is not None:
        ssot = Path(user_root) / '.cc-switch' / 'skills' / SKILL_NAME
        enabled = _flags(row)
        row_items, ssot_ok = _cc_rows(enabled, row, source_state_['hash'], ssot, required)
        items.extend(row_items)
        cc_row_exists = (row is not None and ssot_ok
                         and row.get('content_hash') == source_state_['hash'])
        for host, root in ctx['roots'].items():
            if not root.is_dir():
                items.append(_item('skill-target:' + host, 'SKIP',
                                   '宿主根不存在：%s' % root))
                continue
            target = skill_dir(root)
            target_required = (row is not None or _normalized(target) in marker_paths)
            if enabled.get(host):
                items.append(_target_item('skill-target:' + host,
                                          source_state_['hash'], target,
                                          required=target_required))
            elif not target.is_dir():
                items.append(_item('skill-target:' + host, 'PASS',
                                   '宿主已关闭且无副本：%s' % target))
            elif content_hash(target) == source_state_['hash']:
                items.append(_item('skill-target:' + host, 'WARN',
                                   '宿主已关闭但仍有与来源一致的副本；重跑 64 清理：%s'
                                   % target))
            else:
                items.append(_item('skill-target:' + host, 'WARN',
                                   '宿主已关闭且副本内容不一致，保留：%s' % target))
        items.extend(_unsupported_items(enabled))
    else:
        items.append(_item('skill-cc-switch', 'SKIP',
                           'cc-switch 不在位；使用 standalone 部署'))
        expected = [('agents', skill_dir(Path(user_root) / '.agents'))]
        for host, root in ctx['roots'].items():
            if root.is_dir():
                expected.append((host, skill_dir(root)))
            else:
                items.append(_item('skill-target:' + host, 'SKIP',
                                   '宿主根不存在：%s' % root))
        for host, target in expected:
            items.append(_target_item('skill-target:' + host,
                                      source_state_['hash'], target,
                                      _normalized(target) in marker_paths))
    deepseek_root = Path(user_root) / '.deepseek'
    skills = deepseek_root / 'skills'
    if skills.is_dir():
        deepseek_target = skill_dir(deepseek_root)
        items.append(_target_item('skill-target:deepseek', source_state_['hash'],
                                  deepseek_target,
                                  _normalized(deepseek_target) in marker_paths))
    else:
        items.append(_item('skill-target:deepseek', 'SKIP',
                           'DeepSeek CLI 技能目录不存在：%s' % skills))

    if marker is None:
        items.append(_item('skill-marker', 'WARN',
                           '无部署标记：%s%s'
                           % (marker_path(user_root),
                              '' if not marker_error else '；' + marker_error)))
    else:
        marker_ok = marker_error is None and _marker_consistent(
            marker, source_state_['hash'], user_root, local_appdata)
        if marker_ok:
            items.append(_item('skill-marker', 'PASS',
                               '标记与部署一致：%s' % marker_path(user_root)))
        else:
            items.append(_item('skill-marker', 'FAIL',
                               '标记记录的目标缺失或哈希不一致：%s'
                               % marker_path(user_root)))

    any_fail = any(item['status'] == 'FAIL' for item in items)
    if has_marker:
        status = 'FAIL' if any_fail else 'PASS'
        detail = ('全部目标与 cc-switch 登记一致' if status == 'PASS'
                  else '有目标缺失或哈希漂移；重跑 64')
    elif cc_row_exists and not any_fail:
        status = 'PASS'
        detail = 'cc-switch 已登记且 SSOT/启用目标一致（无部署标记）'
    else:
        status = 'WARN'
        detail = '未部署：无部署标记，cc-switch 也未建立一致登记；运行 64 完成分发'
    items.append(_item('am-skill', status, detail))
    return items


def _marker_consistent(marker, source_hash, user_root, local_appdata=None):
    targets = marker.get('targets')
    if not isinstance(targets, list) or not targets:
        return False
    if marker.get('source_hash') != source_hash:
        return False
    for entry in targets:
        if not isinstance(entry, dict):
            return False
        path = entry.get('path')
        if not isinstance(path, str) or not is_managed_target(path, user_root, local_appdata):
            return False
        target = Path(path)
        if not target.is_dir() or entry.get('hash') != content_hash(target):
            return False
    return True


def audit(source, user_root, local_appdata=None):
    """90 ``AM-SKILL``: the aggregate item first, then the detail items."""
    items = check(source, user_root, local_appdata)
    aggregate = next((item for item in items if item['item'] == 'am-skill'), None)
    details = [item for item in items if item['item'] != 'am-skill']
    if aggregate is None:
        aggregate = _item('am-skill', 'WARN', '技能分发检查没有输出')
    if aggregate['status'] == 'FAIL':
        failed = [item['item'] for item in details if item['status'] == 'FAIL']
        aggregate['detail'] += '（' + ', '.join(failed) + '）'
    return [aggregate] + details


# ---------------------------------------------------------------------------
# Removal
# ---------------------------------------------------------------------------

def remove(user_root, local_appdata=None):
    items = []
    marker, marker_error = read_marker(user_root)
    if marker is None:
        items.append(_item('skill-remove', 'SKIP',
                           '无部署标记，未删除任何副本%s'
                           % ('' if not marker_error else '：' + marker_error)))
    else:
        removed = 0
        kept = 0
        for entry in marker.get('targets') or []:
            if not isinstance(entry, dict):
                continue
            path = entry.get('path')
            if (not isinstance(path, str)
                    or not is_managed_target(path, user_root, local_appdata)):
                kept += 1
                continue
            target = Path(path)
            if not target.exists():
                continue
            if content_hash(target) == entry.get('hash'):
                remove_copy(target)
                removed += 1
            else:
                kept += 1
        items.append(_item('skill-remove', 'PASS' if kept == 0 else 'WARN',
                           '已删除标记记录的副本 %d 个%s'
                           % (removed, '' if kept == 0
                              else '；%d 个副本内容已变化，保留待人工确认' % kept)))

    try:
        snapshot = cc_snapshot(user_root)
    except SkillError as err:
        items.append(_item('skill-remove-cc-switch', 'FAIL', str(err)))
        snapshot = None
    if snapshot is not None:
        row = snapshot['row']
        if row is not None:
            backup = _backup_db(user_root)
            with closing(sqlite3.connect(str(snapshot['db']))) as conn:
                with conn:
                    conn.execute('DELETE FROM skills WHERE id=?', (SKILL_ID,))
            items.append(_item('skill-remove-cc-switch', 'PASS',
                               '已删除 %s 行；备份 %s' % (SKILL_ID, backup)))
        else:
            items.append(_item('skill-remove-cc-switch', 'PASS',
                               'cc-switch 无 %s 行' % SKILL_ID))
        ssot = Path(user_root) / '.cc-switch' / 'skills' / SKILL_NAME
        if ssot.is_dir():
            backup = _backup_ssot(user_root)
            remove_copy(ssot)
            items.append(_item('skill-remove-ssot', 'PASS',
                               '已删除 SSOT；备份 %s' % backup))
        else:
            items.append(_item('skill-remove-ssot', 'PASS',
                               'SSOT 不存在：%s' % ssot))
    else:
        items.append(_item('skill-remove-cc-switch', 'SKIP', 'cc-switch 不在位'))

    path = marker_path(user_root)
    if path.is_file():
        try:
            path.unlink()
            items.append(_item('skill-marker', 'PASS', '已删除部署标记：%s' % path))
        except OSError as err:
            items.append(_item('skill-marker', 'FAIL', '删除部署标记失败：%s' % err))
    else:
        items.append(_item('skill-marker', 'SKIP', '部署标记不存在：%s' % path))
    return items


# ---------------------------------------------------------------------------
# MCP host registration (assist channel)
# ---------------------------------------------------------------------------

def mcp_expected(user_root):
    exe = str(Path(user_root) / '.agent-memory' / 'bin' / 'agent-memory.exe')
    recall = str(Path(user_root) / '.agent-memory' / 'recall.json')
    return exe, recall


def mcp_payload(user_root):
    exe, recall = mcp_expected(user_root)
    return dict(type='stdio', command=exe, args=['mcp', '--config', recall])


def _claude_entry(user_root):
    path = Path(user_root) / '.claude.json'
    if not path.is_file():
        return None, 'missing'
    try:
        payload = json.loads(_text(path))
    except (OSError, ValueError) as err:
        return None, 'unreadable: %s' % err
    if not isinstance(payload, dict):
        return None, 'not an object'
    servers = payload.get('mcpServers')
    if not isinstance(servers, dict):
        return None, 'absent'
    return servers.get(SKILL_NAME), 'ok'


def _codex_entry(user_root):
    path = Path(user_root) / '.codex' / 'config.toml'
    if not path.is_file():
        return None, 'missing'
    try:
        import tomllib
    except ModuleNotFoundError:
        # Python 3.10 has no tomllib: parse only the simple block this module
        # writes, and report anything else as unreadable instead of guessing.
        return _codex_entry_fallback(path)
    try:
        with open(path, 'rb') as handle:
            payload = tomllib.load(handle)
        servers = payload.get('mcp_servers')
        if not isinstance(servers, dict):
            return None, 'absent'
        return servers.get(SKILL_NAME), 'ok'
    except Exception as err:  # noqa: BLE001 - report, never guess
        return None, 'unreadable: %s' % err


def _codex_entry_fallback(path):
    text = _text(path)
    block = re.search(r'(?ms)^\[mcp_servers\.%s\]\s*$(.*?)(?=^\[|\Z)'
                      % re.escape(SKILL_NAME), text)
    if not block:
        return None, 'absent'
    body = block.group(1)
    command = re.search(r'(?m)^\s*command\s*=\s*(.+?)\s*$', body)
    args = re.search(r'(?m)^\s*args\s*=\s*\[(.*?)\]\s*$', body)
    if not command or not args:
        return None, 'unreadable: tomllib is unavailable and the block is not simple'
    def unquote(value):
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
            return value[1:-1]
        return value
    values = [unquote(item) for item in args.group(1).split(',') if item.strip()]
    return dict(command=unquote(command.group(1)), args=values), 'ok'


def _matches(entry, exe, recall):
    if not isinstance(entry, dict):
        return False
    command = entry.get('command')
    args = entry.get('args')
    if isinstance(command, list):
        argv = [str(value) for value in command]
    else:
        argv = [str(command)] + [str(value) for value in (args or [])]
    return argv == [exe, 'mcp', '--config', recall]


def mcp_items(user_root, cc_mode):
    exe, recall = mcp_expected(user_root)
    payload = mcp_payload(user_root)
    hint = ('请在 cc-switch 界面登记 MCP（%s）'
            % json.dumps(payload, ensure_ascii=False)) if cc_mode else \
        '运行 64-install-recall.ps1 -RegisterMcp（无 cc-switch 时）'
    items = []
    for name, (entry, state) in (('mcp-claude', _claude_entry(user_root)),
                                 ('mcp-codex', _codex_entry(user_root))):
        if _matches(entry, exe, recall):
            items.append(_item(name, 'PASS', '已登记且命令一致'))
        else:
            detail = '未登记或命令不一致（%s）；%s' % (state, hint)
            items.append(_item(name, 'WARN', detail))
    return items


def _run_host_command(argv):
    if os.name == 'nt':
        line = subprocess.list2cmdline(argv)
        shell = os.environ.get('COMSPEC') or 'cmd.exe'
        return subprocess.run([shell, '/d', '/c', line], capture_output=True,
                              text=True, encoding='utf-8', errors='replace')
    return subprocess.run(argv, capture_output=True, text=True, encoding='utf-8',
                          errors='replace')


def _register_one(name, executable, argv, existing, exe, recall, hint):
    if _matches(existing, exe, recall):
        return _item(name, 'SKIP', '已登记且命令一致，跳过（%s）' % executable)
    if shutil.which(executable) is None:
        return _item(name, 'SKIP', '未找到 %s 命令，跳过登记' % executable)
    try:
        result = _run_host_command(argv)
    except OSError as err:
        return _item(name, 'WARN', '执行 %s 失败：%s' % (executable, err))
    if result.returncode == 0:
        return _item(name, 'PASS', '已执行：' + subprocess.list2cmdline(argv))
    detail = (result.stderr or result.stdout or '').strip().splitlines()
    return _item(name, 'WARN', '%s 退出 %d：%s'
                 % (executable, result.returncode,
                    (detail[-1] if detail else hint)))


def mcp_register(user_root, cc_mode):
    if cc_mode:
        return [_item('mcp-register', 'SKIP',
                      'cc-switch 在位；不写宿主 MCP 配置，请在 cc-switch 界面登记')]
    exe, recall = mcp_expected(user_root)
    items = []
    claude_entry, _ = _claude_entry(user_root)
    items.append(_register_one(
        'mcp-register-claude', 'claude',
        ['claude', 'mcp', 'add', '-s', 'user', SKILL_NAME, '--',
         exe, 'mcp', '--config', recall],
        claude_entry, exe, recall, '检查 claude mcp list'))
    codex_entry, _ = _codex_entry(user_root)
    items.append(_register_one(
        'mcp-register-codex', 'codex',
        ['codex', 'mcp', 'add', SKILL_NAME, '--', exe, 'mcp', '--config', recall],
        codex_entry, exe, recall, '检查 codex mcp list'))
    return items
