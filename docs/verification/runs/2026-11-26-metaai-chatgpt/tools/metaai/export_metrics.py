"""Export measured numeric evidence only; never read prompts or response logs."""
import csv
import json
import pathlib
import re
import zipfile

ROOT = pathlib.Path(__file__).parent
report = json.loads((ROOT / 'metrics-report.json').read_text())
source = pathlib.Path('/Users/fangdu/projects/Chat-Gateway/src/metaai_timing.rs').read_text()
catalog = re.findall(r'Self::\w+ => "([a-z_]+)"', source)
assert len(catalog) == len(set(catalog)) == 44
children = {
    'navigation': ['navigation_before_request', 'navigation_fetch', 'navigation_after_response'],
    'submission': ['input_fill', 'input_settle', 'draft_validation', 'send_dispatch', 'submission_ack'],
    'input_fill': ['paste_dispatch', 'input_retention'],
    'input_retention': ['retention_probe', 'retention_settle', 'retention_sleep'],
    'input_settle': ['input_settle_engine', 'input_settle_sleep'],
    'submission_ack': ['ack_probe', 'ack_settle', 'ack_sleep'],
    'composer_ready': ['composer_wait', 'composer_probe', 'composer_settle', 'composer_sleep'],
}
atomic = {
    'navigation_fetch': 'First-party fetch boundary; DNS/connect/TLS/TTFB/body child durations unavailable.',
    'navigation_after_response': 'Native navigation boundary; parse/script/layout/event-loop children unavailable.',
    'navigation_unobserved': 'Navigation observer did not expose native child events.',
    'retention_settle': 'One native engine drive call; cannot divide its elapsed time without engine spans.',
    'input_settle_engine': 'One native engine drive call; internal task attribution unavailable.',
    'first_content_settle': 'Native drive calls while awaiting content; remote and local work not separated.',
    'after_content_settle': 'Native drive calls during generation and termination; remote and local work not separated.',
    'browser_settle': 'Native drive call; internal task attribution unavailable.',
    'composer_settle': 'Native drive call; internal task attribution unavailable.',
    'ack_settle': 'Native drive call; internal task attribution unavailable.',
}


def write_csv(name, rows, fields):
    with (ROOT / name).open('w', newline='') as out:
        writer = csv.DictWriter(out, fieldnames=fields, extrasaction='ignore')
        writer.writeheader()
        writer.writerows(rows)


calls = []
stages = []
series = []
for index, job in enumerate(report['jobs'], 1):
    for request in job['requests']:
        calls.append({'call': index, 'job': job['job'], **request,
                      'cli_seconds': job['cli_seconds'],
                      'image_identity_observed_stable': job['image_identity_observed_stable'],
                      'proxy_rejected_budget_posts':job.get('proxy_rejected_budget_posts'),
                      'coverage_gaps': ';'.join(job['coverage_gaps'])})
    for row in job['stages']:
        if row['operations']:
            stages.append({'call': index, 'job': job['job'], **row,
                           'measured_children': ';'.join(children.get(row['stage'], [])),
                           'boundary': atomic.get(row['stage'], ''),
                           'scope': 'before/after shared-container window; nested parents overlap'})
    for row in job['series']:
        series.append({'call': index, 'job': job['job'], **row,
                       'scope':row.get('scope','shared_window_counter_delta'),
                       'labels': json.dumps(row['labels'], sort_keys=True)})

write_csv('calls.csv', calls, ['call', 'job', 'request_id', 'status', 'protocol_complete',
    'input_chars', 'input_chars_scope', 'input_payload_json_bytes', 'non_string_content_seen', 'proxy_rejected_budget_posts', 'output_chars', 'content_deltas', 'headers_s', 'first_content_s',
    'generation_seconds', 'tail_wait_s', 'total_s', 'cli_seconds', 'max_inter_delta_gap_s',
    'metrics_before_collection_s', 'metrics_after_collection_s',
    'image_identity_observed_stable', 'coverage_gaps'])
write_csv('browser-stages.csv', stages, ['call', 'job', 'stage', 'operations',
    'sum_seconds', 'mean_operation_seconds', 'operations_over_one_second',
    'measured_children', 'boundary', 'scope'])
write_csv('all-metric-series.csv', series, ['call', 'job', 'name', 'labels',
    'before', 'after', 'delta', 'scope'])

summary = []
for stage in catalog:
    rows = [r for r in stages if r['stage'] == stage]
    summary.append({'stage': stage, 'observed_windows': len(rows),
        'min_window_sum_s': min((r['sum_seconds'] for r in rows), default=None),
        'max_window_sum_s': max((r['sum_seconds'] for r in rows), default=None),
        'operations_over_one_second': sum(r['operations_over_one_second'] for r in rows),
        'children': children.get(stage, []), 'atomic_boundary': atomic.get(stage),
        'note': 'Window sums are repeated work, not individual latency maxima; parents overlap.'})
(ROOT / 'stage-catalog.json').write_text(json.dumps(summary, indent=2) + '\n')
(ROOT / 'cohort-summary.json').write_text(json.dumps(report['cohort'], indent=2) + '\n')
plan_file=ROOT/'improvement-plan.json'
if plan_file.exists():
    plan=json.loads(plan_file.read_text());plan['observed_cohort']=report['cohort']
    plan_file.write_text(json.dumps(plan,indent=2)+'\n')
with zipfile.ZipFile(ROOT / 'telemetry-export.zip', 'w', zipfile.ZIP_DEFLATED) as out:
    for name in ['metrics-report.json', 'metrics-summary.log', 'calls.csv',
                 'browser-stages.csv', 'all-metric-series.csv', 'stage-catalog.json',
                 'cohort-summary.json', 'improvement-plan.json']:
        out.write(ROOT / name, arcname=name)
print(json.dumps({'calls': len(calls), 'catalog_stages': len(catalog),
                  'stage_windows': len(stages), 'metric_series': len(series)}))
