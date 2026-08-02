import asyncio
import argparse
import json
import socket

import websockets

async def connect_i2c(url, attempts=5, delay=0.5):
    last_error = None

    for attempt in range(1, attempts + 1):
        try:
            return await websockets.connect(
                url,
                family=socket.AF_INET,
                open_timeout=2,
            )
        except TimeoutError as error:
            last_error = error
            if attempt == attempts:
                break
            await asyncio.sleep(delay)

    raise last_error


async def main(url):
    async with await connect_i2c(url) as ws:
        print("hello:", await ws.recv())

        await ws.send(json.dumps({"type": "i2c.status"}))
        print("status:", await ws.recv())

        await ws.send(json.dumps({"type": "i2c.scan", "bus": 0}))
        print("scan:", await ws.recv())


parser = argparse.ArgumentParser()
parser.add_argument("--host", required=True, help="UID-suffixed hostname or IP address")
args = parser.parse_args()
asyncio.run(main(f"ws://{args.host}/i2c"))
