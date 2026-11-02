"""Owner-run loopback-to-loopback SSH stdio bridge, not a public web proxy.

Run on the owner's computer after establishing/trusting the connection shown
by Replit's SSH tool. No credentials are accepted, read, stored or logged here.
The existing gateway API authorization remains required inside the app.
"""
import argparse
import asyncio
import contextlib
import re
import shlex
import shutil
import signal

# Fixed remote command: only connect to the gateway's loopback socket. No
# session/profile/filesystem access, header rewriting or TLS-proxy assertions.
REMOTE = """import os,select,socket
s=socket.create_connection(('127.0.0.1',8099),timeout=15)
s.settimeout(None)
inputs=[0,s]
while s in inputs:
 for source in select.select(inputs,[],[])[0]:
  data=os.read(0,65536) if source==0 else s.recv(65536)
  if not data:
   inputs.remove(source)
   if source==0: s.shutdown(socket.SHUT_WR)
   continue
  if source==0: s.sendall(data)
  else:
   while data: data=data[os.write(1,data):]
s.close()
"""


def ssh_command(target, port, gateway_port=8099, config=None):
    if (not re.fullmatch(r"[A-Za-z0-9_.@\[\]:-]+", target)
            or target.startswith("-") or target.count("@") > 1):
        raise ValueError("Use the SSH target or host alias from the SSH tool.")
    if not 1 <= port <= 65535:
        raise ValueError("SSH port must be between 1 and 65535.")
    if not 1 <= gateway_port <= 65535:
        raise ValueError("Gateway port must be between 1 and 65535.")
    return ["ssh", *(["-F", config] if config else []),
        "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
        "-o", "ConnectTimeout=15",
        "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=3",
        "-p", str(port), target,
        "python3 -c " + shlex.quote(REMOTE.replace("8099", str(gateway_port))),
    ]


async def bridge(reader, writer, command):
    process = None
    pumps = []
    try:
        process = await asyncio.create_subprocess_exec(
            *command, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)

        async def upstream():
            while data := await reader.read(65536):
                process.stdin.write(data)
                await process.stdin.drain()
            if not process.stdin.is_closing():
                process.stdin.close()

        async def downstream():
            while data := await process.stdout.read(65536):
                writer.write(data)
                await writer.drain()

        pumps = [asyncio.create_task(upstream()), asyncio.create_task(downstream())]
        # The remote endpoint can finish before the browser closes keepalive.
        done, _ = await asyncio.wait(pumps, return_when=asyncio.FIRST_COMPLETED)
        if pumps[0] in done and not pumps[1].done():
            await asyncio.wait_for(pumps[1], timeout=15)
        for task in done:
            task.result()
    except (OSError, ConnectionError, asyncio.TimeoutError):
        # Never print raw SSH output, requests, headers, frames or input.
        pass
    finally:
        for task in pumps:
            task.cancel()
        await asyncio.gather(*pumps, return_exceptions=True)
        if process and process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                process.terminate()
            try:
                await asyncio.wait_for(process.wait(), timeout=3)
            except asyncio.TimeoutError:
                with contextlib.suppress(ProcessLookupError):
                    process.kill()
                await process.wait()
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()


async def serve(command, port=8099):
    slots = asyncio.Semaphore(12)

    async def accept(reader, writer):
        if slots.locked():
            writer.close()
            return
        async with slots:
            await bridge(reader, writer, command)

    server = await asyncio.start_server(accept, "127.0.0.1", port)
    bound_port = server.sockets[0].getsockname()[1]
    print(f"Owner-only local UI: http://127.0.0.1:{bound_port}/login", flush=True)
    print("SSH authentication and gateway authorization are both required.", flush=True)
    with contextlib.suppress(NotImplementedError):
        asyncio.get_running_loop().add_signal_handler(
            signal.SIGTERM, asyncio.current_task().cancel)
    async with server:
        await server.serve_forever()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ssh-target", required=True, help="Target/alias from Replit SSH tool")
    parser.add_argument("--ssh-port", type=int, default=22)
    parser.add_argument("--ssh-config", help="Optional owner-local OpenSSH config path")
    parser.add_argument("--gateway-port", type=int, default=8099)
    parser.add_argument("--local-port", type=int, default=8099)
    args = parser.parse_args()
    try:
        command = ssh_command(args.ssh_target, args.ssh_port,
                              args.gateway_port, args.ssh_config)
        if not 0 <= args.local_port <= 65535:
            raise ValueError("Invalid local port.")
        if not shutil.which("ssh"):
            parser.error("OpenSSH client is required on the owner's computer.")
        asyncio.run(serve(command, args.local_port))
    except (OSError, ValueError):
        parser.exit(1, "Tunnel unavailable. Check the SSH tool, trusted host, agent and local port.\n")
    except (KeyboardInterrupt, asyncio.CancelledError):
        pass


if __name__ == "__main__":
    main()
