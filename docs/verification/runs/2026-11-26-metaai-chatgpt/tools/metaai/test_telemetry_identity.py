import unittest,json,subprocess
from types import SimpleNamespace
from unittest.mock import patch
from telemetry_identity import docker_image_identity
class IdentityTests(unittest.TestCase):
    def good(self):return {'image_id':'sha256:'+'a'*64,'source_sha':'b'*40,'ports':[{'HostIp':'127.0.0.1','HostPort':'8080'}],'private':'SECRET'}
    def test_only_validated_identity_is_exported(self):
        with patch('telemetry_identity.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps(self.good()))) as run:
            r=docker_image_identity();self.assertTrue(r['available']);self.assertNotIn('SECRET',json.dumps(r));self.assertNotIn('ports',r)
            args=run.call_args;self.assertEqual(args.kwargs['timeout'],2);self.assertNotIn('.Config.Env',args.args[0][-1]);self.assertNotIn('shell',args.kwargs)
    def test_invalid_identity_and_failures_never_echo_private_input(self):
        cases=[dict(self.good(),source_sha='SECRET'),dict(self.good(),image_id='SECRET'),dict(self.good(),ports=[{'HostIp':'0.0.0.0','HostPort':'8080'}])]
        for value in cases:
            with patch('telemetry_identity.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps(value))):
                r=docker_image_identity();self.assertEqual(r,{'available':False,'reason':'identity_unavailable'});self.assertNotIn('SECRET',json.dumps(r))
        with patch('telemetry_identity.subprocess.run',side_effect=subprocess.TimeoutExpired('private',2)):
            self.assertEqual(docker_image_identity(),{'available':False,'reason':'identity_unavailable'})
