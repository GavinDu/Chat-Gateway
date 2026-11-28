"""Exercise the actual proxy handler offline; no OpenCode/provider dispatch."""
import ast
import concurrent.futures
import io
import json
import pathlib
import threading
import time
import unittest


def fixture(payload, denials, inner_snapshot=True):
    calls, errors, snapshots = [], [], []
    all_denied = threading.Event()
    result_lock = threading.Lock()

    class Base:
        def __init__(self):
            self.path = '/v1/chat/completions'
            self.rfile = io.BytesIO(payload)
            self.headers = {'Content-Length': str(len(payload)),
                            'X-CG-Bench-Kind': 'primary'}

        def send_error(self, status, reason):
            with result_lock:
                errors.append(status)
                if len(errors) == denials:
                    all_denied.set()

    def stream(body, record, sink):
        calls.append(True)
        record['_wire_start'] = time.monotonic()
        if denials and not all_denied.wait(5):
            raise RuntimeError('offline test did not observe expected denials')
        if inner_snapshot:
            record['metrics_after_collection_s'] = 0.001

    def snapshot(name):
        snapshots.append(name)
        return 0.002

    source = pathlib.Path(__file__).with_name('run_opencode.py')
    tree = ast.parse(source.read_text())
    handler = next(node for node in tree.body
                   if isinstance(node, ast.ClassDef) and node.name == 'Proxy')
    namespace = dict(BaseHTTPRequestHandler=Base, json=json, time=time,
                     records=[], rejected_posts=0,
                     request_lock=threading.Lock(), request_finished=threading.Event(),
                     stream=stream, snapshot=snapshot)
    exec(compile(ast.Module(body=[handler], type_ignores=[]), str(source), 'exec'), namespace)
    return namespace, calls, errors, snapshots


class ProxyMeasurementTests(unittest.TestCase):
    def test_concurrent_clients_forward_once_count_denials_and_keep_snapshot(self):
        payload = json.dumps({'messages': [{'role': 'user', 'content': 'synthetic'}],
                              'stream': True}).encode()
        namespace, calls, errors, snapshots = fixture(payload, denials=15)
        barrier = threading.Barrier(16)

        def request():
            barrier.wait(timeout=5)
            namespace['Proxy']().do_POST()

        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            futures = [pool.submit(request) for _ in range(16)]
            for future in futures:
                future.result(timeout=8)
        self.assertEqual(len(calls), 1)
        self.assertEqual(errors, [429] * 15)
        self.assertEqual(namespace['rejected_posts'], 15)
        self.assertEqual(len(namespace['records']), 1)
        self.assertEqual(snapshots, [])  # The inner snapshot must not be overwritten.
        self.assertTrue(namespace['request_finished'].is_set())

    def test_missing_inner_snapshot_has_one_fallback_and_size_scope_is_explicit(self):
        payload = json.dumps({'messages': [{'content': 'é'}, {'content': [
            {'type': 'image_url', 'image_url': {'url': 'PRIVATE'}}]}]},
            ensure_ascii=False).encode()
        namespace, calls, errors, snapshots = fixture(payload, denials=0, inner_snapshot=False)
        namespace['Proxy']().do_POST()
        record = namespace['records'][0]
        self.assertEqual(len(calls), 1)
        self.assertEqual(errors, [])
        self.assertEqual(len(snapshots), 1)
        self.assertEqual(record['metrics_after_collection_s'], 0.002)
        self.assertEqual(record['input_payload_json_bytes'], len(payload))
        self.assertEqual(record['input_chars'], 1)
        self.assertEqual(record['input_chars_scope'], 'string_message_content_only')
        self.assertTrue(record['non_string_content_seen'])
        self.assertNotIn('PRIVATE', json.dumps(record))


if __name__ == '__main__':
    unittest.main()
