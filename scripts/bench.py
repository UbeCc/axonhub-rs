#!/usr/bin/env python3
"""Latency/throughput benchmark for a Messages-API endpoint.

Compares gateways against the same instant mock upstream so the numbers are
dominated by proxy overhead, not by model generation.

Usage:
    python3 bench.py URL API_KEY MODEL [N] [CONCURRENCY] [--stream]
"""
import asyncio
import statistics
import sys
import time

import httpx

URL = sys.argv[1]
KEY = sys.argv[2]
MODEL = sys.argv[3]
N = int(sys.argv[4]) if len(sys.argv) > 4 else 1000
CONCURRENCY = int(sys.argv[5]) if len(sys.argv) > 5 else 50
STREAM = "--stream" in sys.argv
BODY = {
    "model": MODEL,
    "max_tokens": 8,
    "messages": [{"role": "user", "content": "hi"}],
}
if STREAM:
    BODY["stream"] = True
HEADERS = {"x-api-key": KEY, "content-type": "application/json", "anthropic-version": "2023-06-01"}


async def one(client: httpx.AsyncClient, sem: asyncio.Semaphore, samples: list, ttfb: list):
    async with sem:
        start = time.perf_counter()
        if STREAM:
            async with client.stream("POST", URL, json=BODY, headers=HEADERS) as response:
                if response.status_code != 200:
                    raise RuntimeError(f"status {response.status_code}")
                async for _ in response.aiter_raw():
                    ttfb.append(time.perf_counter() - start)
                    break
        else:
            response = await client.post(URL, json=BODY, headers=HEADERS)
            if response.status_code != 200:
                raise RuntimeError(f"status {response.status_code}: {response.text[:200]}")
        samples.append(time.perf_counter() - start)


async def main():
    limits = httpx.Limits(max_connections=CONCURRENCY, max_keepalive_connections=CONCURRENCY)
    async with httpx.AsyncClient(limits=limits, timeout=30, trust_env=False) as client:
        sem = asyncio.Semaphore(CONCURRENCY)
        for _ in range(min(20, N)):
            await one(client, sem, [], [])
        samples: list = []
        ttfb: list = []
        started = time.perf_counter()
        await asyncio.gather(*(one(client, sem, samples, ttfb) for _ in range(N)))
        elapsed = time.perf_counter() - started
    samples.sort()
    ttfb.sort()

    def pct(values, p):
        if not values:
            return float("nan")
        return values[min(len(values) - 1, int(len(values) * p))]

    label = "stream" if STREAM else "json"
    print(
        f"{label} n={N} c={CONCURRENCY} rps={N / elapsed:.1f} "
        f"mean={statistics.mean(samples) * 1000:.2f}ms "
        f"p50={pct(samples, 0.50) * 1000:.2f}ms "
        f"p90={pct(samples, 0.90) * 1000:.2f}ms "
        f"p99={pct(samples, 0.99) * 1000:.2f}ms "
        f"max={samples[-1] * 1000:.2f}ms "
        + (f"ttfb_p50={pct(ttfb, 0.50) * 1000:.2f}ms ttfb_p99={pct(ttfb, 0.99) * 1000:.2f}ms" if STREAM else "")
    )


if __name__ == "__main__":
    asyncio.run(main())
