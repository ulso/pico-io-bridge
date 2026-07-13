import asyncio
import json
import socket

import websockets

URL = "ws://pico-io-can-feather.local/i2c"


async def connect_i2c(url=URL, attempts=5, delay=0.5):
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


async def main():
    async with await connect_i2c() as ws:
        print("hello:", await ws.recv())

        await ws.send(json.dumps({"type": "i2c.status"}))
        print("status:", await ws.recv())

        await ws.send(json.dumps({"type": "i2c.scan", "bus": 0}))
        print("scan:", await ws.recv())


asyncio.run(main())
