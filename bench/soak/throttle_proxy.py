#!/usr/bin/env python3
"""Dry run only: an HTTP/1.1 proxy in front of the MinIO sidecar that answers every Nth PUT or
GET with `503 SlowDown` (or another status and code), so that S9 can be tried without a real endpoint that throttles.

    throttle_proxy.py --listen 127.0.0.1:9100 --upstream minio:9000 --every 40 --stats FILE
                      [--status 429 --code TooManyRequests]

Requests are passed on unchanged (the Host header too, which the signature covers). It listens
on loopback inside the soak container and handles plain HTTP with Content-Length or chunked
bodies, which is all the dry run needs.
"""
import argparse
import asyncio
import json
import signal

SLOW = b""
stats = {"requests": 0, "injected": 0}


def make_reply(status, code):
    body = (b'<?xml version="1.0" encoding="UTF-8"?><Error><Code>' + code.encode() + b"</Code>"
            b"<Message>Please reduce your request rate.</Message><RequestId>soak</RequestId>"
            b"<HostId>soak</HostId></Error>")
    return (f"HTTP/1.1 {status} {code}\r\nContent-Type: application/xml\r\nContent-Length: {len(body)}\r\n"
            "x-amz-request-id: soak\r\n\r\n").encode() + body


def parse(head):
    lines = head.split(b"\r\n")
    hdrs = {}
    for ln in lines[1:]:
        if b":" in ln:
            k, v = ln.split(b":", 1)
            hdrs[k.strip().lower()] = v.strip()
    return lines[0], hdrs


async def relay_chunked(src, dst):
    while True:
        line = await src.readline()
        if dst:
            dst.write(line)
        n = int(line.split(b";")[0].strip() or b"0", 16)
        if n == 0:
            while True:   # trailers, up to the empty line
                t = await src.readline()
                if dst:
                    dst.write(t)
                if t in (b"\r\n", b"\n", b""):
                    return
        data = await src.readexactly(n + 2)
        if dst:
            dst.write(data)


async def relay_body(hdrs, src, dst):
    if hdrs.get(b"transfer-encoding", b"").lower() == b"chunked":
        await relay_chunked(src, dst)
    else:
        left = int(hdrs.get(b"content-length", b"0"))
        while left:
            data = await src.read(min(left, 1 << 16))
            if not data:
                raise ConnectionError("short body")
            if dst:
                dst.write(data)
            left -= len(data)
    if dst:
        await dst.drain()


async def handle(reader, writer, upstream, every):
    up_r = up_w = None
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            first, hdrs = parse(head)
            method = first.split(b" ", 1)[0]
            stats["requests"] += 1
            if method in (b"PUT", b"GET") and stats["requests"] % every == 0:
                stats["injected"] += 1
                if hdrs.get(b"expect", b"").lower() != b"100-continue":
                    await relay_body(hdrs, reader, None)   # read and drop the body
                    writer.write(SLOW)
                    await writer.drain()
                    continue
                writer.write(SLOW.replace(b"\r\n\r\n", b"\r\nConnection: close\r\n\r\n", 1))
                await writer.drain()
                return
            if up_w is None:
                host, port = upstream.rsplit(":", 1)
                up_r, up_w = await asyncio.open_connection(host, int(port))
            up_w.write(head)
            await relay_body(hdrs, reader, up_w)
            while True:
                rhead = await up_r.readuntil(b"\r\n\r\n")
                rfirst, rh = parse(rhead)
                status = int(rfirst.split(b" ", 2)[1])
                writer.write(rhead)
                if status != 100:
                    break
            if method != b"HEAD" and status not in (204, 304):
                if b"content-length" in rh or rh.get(b"transfer-encoding", b"").lower() == b"chunked":
                    await relay_body(rh, up_r, writer)
                else:   # no framing: the body ends with the connection
                    while True:
                        data = await up_r.read(1 << 16)
                        if not data:
                            return
                        writer.write(data)
                        await writer.drain()
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError, OSError):
        pass
    finally:
        for w in (writer, up_w):
            if w is not None:
                w.close()


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", required=True)
    ap.add_argument("--upstream", required=True)
    ap.add_argument("--every", type=int, default=40)
    ap.add_argument("--stats")
    ap.add_argument("--status", type=int, default=503)
    ap.add_argument("--code", default="SlowDown")
    a = ap.parse_args()
    global SLOW
    SLOW = make_reply(a.status, a.code)
    host, port = a.listen.rsplit(":", 1)
    server = await asyncio.start_server(lambda r, w: handle(r, w, a.upstream, a.every), host, int(port))
    stop = asyncio.Event()
    asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, stop.set)
    async with server:
        await stop.wait()
    if a.stats:
        with open(a.stats, "w") as f:
            json.dump(stats, f)


if __name__ == "__main__":
    asyncio.run(main())
