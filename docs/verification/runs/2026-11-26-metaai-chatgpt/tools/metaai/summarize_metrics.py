"""Summarize isolated coding calls; no prompts, responses, or secret stores read."""
import json
import math
import pathlib
import re
import statistics
from cohort_metrics import aggregate_calls

ROOT = pathlib.Path(__file__).parent
LINE = re.compile(r'^([a-zA-Z_:][a-zA-Z_0-9:]*)(?:\{(.*)\})?\s+([-+0-9.eE]+)$')
LABEL = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*")')


def parse(path):
    if not path.exists():
        return None
    result = {}
    for line in path.read_text().splitlines():
        match = LINE.match(line)
        if match:
            name, raw, value = match.groups()
            labels = tuple(sorted((k, json.loads(v)) for k, v in LABEL.findall(raw or '')))
            result[name, labels] = float(value)
    return result


def size(value):
    match = re.match(r'([\d.]+)\s*([KMGT]?i?B)', value)
    if not match:
        return None
    amount, unit = match.groups()
    units = {'B': 1, 'kB': 1000, 'KB': 1000, 'MB': 1e6, 'GB': 1e9,
             'KiB': 1024, 'MiB': 1024**2, 'GiB': 1024**3, 'TiB': 1024**4}
    return float(amount) * units[unit]


jobs = []
for file in ROOT.glob('*/result.json'):
    data = json.loads(file.read_text())
    if data.get('workflow') != 'opencode_metaai_scoped_patch':
        continue
    job = {'job': file.parent.name, 'cli_seconds': data.get('cli_seconds'),
           'exit_code': data.get('exit_code'), 'runtime': data.get('runtime_identity'),
           'requests': data.get('requests', []), 'coverage_gaps': [], 'series': [], 'stages': []}
    job['work_class'] = file.parent.name
    job['warm_state'] = 'unknown; navigation observed where step snapshots are available'
    job['runtime_after'] = data.get('runtime_identity_after')
    job['image_identity_observed_stable'] = data.get('image_identity_observed_stable', False)
    if not job['image_identity_observed_stable']:
        job['coverage_gaps'].append('image_digest_not_attested_per_call')
    if not data.get('single_after_snapshot') and (file.parent/'telemetry/request-1.after.prom').exists():
        job['coverage_gaps'].append('legacy_finalizer_overwrote_after_snapshot; exact_skew_unmeasured')
    job['proxy_rejected_budget_posts'] = data.get('proxy_rejected_budget_posts')
    if job['proxy_rejected_budget_posts'] is None:
        job['coverage_gaps'].append('client_budget_rejections_unobserved')
    if job['runtime'] is None:
        job['coverage_gaps'].append('legacy_job_missing_per_call_health_identity')
    for request in job['requests']:
        request['protocol_complete'] = bool(data.get('exit_code') == 0
            and request.get('status') == 200 and request.get('finish_reason') == 'stop'
            and request.get('done_s') is not None and not request.get('stream_error'))
        if 'last_content_s' in request:
            request['generation_seconds'] = round(request['last_content_s'] - request['first_content_s'], 6)
            request['termination_gap_class'] = 'normal_tail' if request['protocol_complete'] else 'error_termination_gap'
        before = parse(file.parent / 'telemetry/request-1.before.prom')
        after = parse(file.parent / 'telemetry/request-1.after.prom')
        if before is None or after is None:
            job['coverage_gaps'].append('missing_before_or_after_metrics')
            continue
        deltas = {}
        for key, value in sorted(after.items()):
            name, labels = key
            previous = before.get(key, 0)
            gauge = name in {'gateway_metaai_step_max_seconds', 'gateway_provider_session_state'}
            row = {'name': name, 'labels': dict(labels), 'before': previous, 'after': value}
            if gauge:
                row['scope'] = 'process_lifetime_max' if name.endswith('_max_seconds') else 'snapshot_state'
            elif value < previous:
                row['scope'] = 'counter_reset_unattributable'
                job['coverage_gaps'].append('counter_reset')
            else:
                row['delta'] = value - previous
                deltas[key] = value - previous
            job['series'].append(row)
        stages = sorted({dict(labels)['stage'] for name, labels in after
                         if name == 'gateway_metaai_step_duration_seconds_count'})
        for stage in stages:
            labels = (('stage', stage),)
            count = deltas.get(('gateway_metaai_step_duration_seconds_count', labels), 0)
            total = deltas.get(('gateway_metaai_step_duration_seconds_sum', labels), 0)
            le_one = deltas.get(('gateway_metaai_step_duration_seconds_bucket', tuple(sorted([('stage', stage), ('le', '1')]))), 0)
            job['stages'].append({'stage': stage, 'operations': count, 'sum_seconds': total,
                'mean_operation_seconds': total / count if count else None,
                'operations_over_one_second': max(0, count - le_one)})
    resources = file.parent / 'telemetry/resources.jsonl'
    rows = [json.loads(line) for line in resources.read_text().splitlines()] if resources.exists() else []
    cpu = [float(r['CPUPerc'].strip('%')) for r in rows if r.get('CPUPerc')]
    memory = [size(r['MemUsage'].split('/')[0].strip()) for r in rows if r.get('MemUsage')]
    job['resources'] = {'samples': len(rows), 'cpu_percent_max': max(cpu, default=None),
        'container_working_set_bytes_max': max((m for m in memory if m is not None), default=None),
        'pids_max': max((int(r['PIDs']) for r in rows if r.get('PIDs')), default=None),
        'rss_measured': False, 'scope': 'shared production container, not isolated browser process'}
    if not rows:
        job['coverage_gaps'].append('missing_resource_samples')
    if rows:
        offsets=[r['monotonic_offset_s'] for r in rows]
        intervals=[b-a for a,b in zip(offsets,offsets[1:]) if b>a]
        job['resources']['sample_interval_seconds']={'n':len(intervals),'min':min(intervals,default=None),'median':statistics.median(intervals) if intervals else None,'max':max(intervals,default=None)}
        job['resources']['first_sample_offset_s'] = rows[0]['monotonic_offset_s']
        job['resources']['last_sample_offset_s'] = rows[-1]['monotonic_offset_s']
        job['resources']['net_io_first_last'] = [rows[0]['NetIO'], rows[-1]['NetIO']]
        job['resources']['block_io_first_last'] = [rows[0]['BlockIO'], rows[-1]['BlockIO']]
    applied = file.parent / 'applied.json'
    job['review'] = json.loads(applied.read_text()) if applied.exists() else {'implementation_applied': False}
    jobs.append(job)

jobs.sort(key=lambda j: j['requests'][0].get('started_utc', '') if j['requests'] else '')
report = {'schema_version': 1, 'scope': 'real OpenCode 2.4.0 CLI, Meta AI scoped source tasks',
          'declared_browser_stage_count': 44,
          'declared_stage_without_any_series_in_this_run': ['navigation_unobserved'],
          'jobs': jobs, 'job_count': len(jobs), 'provider_calls': sum(len(j['requests']) for j in jobs),
          'protocol_complete_calls': sum(r['protocol_complete'] for j in jobs for r in j['requests']),
          'notes': ['All failed calls retained. No success-rate SLO or paired causal comparison claimed.',
                    'Histogram stage parents overlap children; never sum them into an E2E total.',
                    'Histogram means and repeated operation totals are not per-request percentile estimates.',
                    'Output characters, SSE deltas and estimated token counts are distinct quantities.',
                    'Process-lifetime maximum gauges cannot be assigned to a single request.',
                    'Client proxy observations are not OpenCode TUI render timing.',
                    'Missing final snapshot on the old 120-second client deadline remains a coverage gap.']}
report['cohort'] = aggregate_calls([r for j in jobs for r in j['requests']])
(ROOT / 'metrics-report.json').write_text(json.dumps(report, indent=2, allow_nan=False) + '\n')
for job in jobs:
    for r in job['requests']:
        print(json.dumps({'job': job['job'], 'status': r.get('status'), 'complete': r['protocol_complete'],
                          'first': r.get('first_content_s'), 'generation': r.get('generation_seconds'),
                          'tail_or_error_gap': r.get('tail_wait_s'), 'total': r.get('total_s'),
                          'gaps': job['coverage_gaps']}))
