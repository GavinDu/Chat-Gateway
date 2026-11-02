"""Transport fixtures only: no SSH credentials, provider pages or real sessions."""
import asyncio
import sys
import unittest
from owner_tunnel import bridge, ssh_command, REMOTE


class CommandTests(unittest.TestCase):
    def test_authentication_and_host_trust_are_not_bypassed(self):
        command = ssh_command("owner-fixture", 2222)
        self.assertIn("BatchMode=yes", command)
        self.assertIn("StrictHostKeyChecking=yes", command)
        self.assertIn("-T", command)
        self.assertNotIn("-L", command)  # No AllowTcpForwarding dependency.
        self.assertIn("127.0.0.1", REMOTE)
        self.assertNotIn("0.0.0.0", REMOTE)

    def test_targets_cannot_inject_options_or_commands(self):
        for target, port in [("-oProxyCommand=bad", 22), ("bad host", 22),
                             ("fixture;false", 22), ("$(fixture)", 22),
                             ("fixture", 0), ("fixture", 65536)]:
            with self.assertRaises(ValueError):
                ssh_command(target, port)


class PipeTests(unittest.IsolatedAsyncioTestCase):
    def test_explicit_ports_and_config_never_rewrite_headers(self):
        command = ssh_command("fixture", 2222, 8199, "/tmp/owner-config")
        self.assertEqual(command[:3], ["ssh", "-F", "/tmp/owner-config"])
        self.assertIn("8199", command[-1])
        self.assertNotIn("8099", command[-1])
        with self.assertRaises(ValueError):
            ssh_command("fixture", 22, 0)

    async def test_raw_http_and_websocket_bytes_preserved(self):
        echo = [sys.executable, "-u", "-c",
                "import os\nwhile True:\n d=os.read(0,65536)\n"
                " if not d: break\n os.write(1,d)\n"]
        tasks = set()

        def accept(reader, writer):
            task = asyncio.create_task(bridge(reader, writer, echo))
            tasks.add(task)
            task.add_done_callback(tasks.discard)

        server = await asyncio.start_server(accept, "127.0.0.1", 0)
        writer = None
        try:
            port = server.sockets[0].getsockname()[1]
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            wire = (b"GET /login HTTP/1.1\r\nHost: 127.0.0.1:8099\r\n"
                    b"Origin: http://127.0.0.1:8099\r\nUpgrade: websocket\r\n\r\n"
                    b"\x82\x04\x00\xff\x80\x01")
            writer.write(wire)
            await writer.drain()
            self.assertEqual(await asyncio.wait_for(reader.readexactly(len(wire)), 3), wire)
            writer.write_eof()
            self.assertEqual(await asyncio.wait_for(reader.read(), 3), b"")
        finally:
            if writer:
                writer.close()
                await writer.wait_closed()
            server.close()
            await server.wait_closed()
            await asyncio.gather(*tasks)
        self.assertFalse(tasks)

    async def test_failed_transport_closes_without_forwarding_stderr(self):
        server = await asyncio.start_server(
            lambda r, w: bridge(r, w, [sys.executable, "-c",
                "import sys;sys.stderr.write('synthetic-private-detail');sys.exit(1)"]),
            "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1])
            self.assertEqual(await asyncio.wait_for(reader.read(), 3), b"")
            writer.close()
            await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()


if __name__ == "__main__":
    unittest.main()
