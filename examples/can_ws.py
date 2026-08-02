import asyncio
import argparse
import websockets
import socket
import json

async def connect_can(url, attempts=5, delay=0.5):
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


async def set_other_led(sock, on):
    frame = {
        "type": "can.tx",
        "bus":0,
        "id":291,
        "ext":False,
        "rtr":False,
        "dlc":1,
        "data":[on]
    }
    await sock.send(json.dumps(frame))

        
async def main(url):
    async with await connect_can(url) as ws:
        print("hello:", await ws.recv())
        await ws.send('{"type":"can.status"}')
        print("status:", await ws.recv())
        
        print()
        
        print("Turn on LED on other node...")
        await set_other_led(ws,1)
        print("tx:", await ws.recv())

        await asyncio.sleep(1)
        
        print("Turn off LED on other node...")
        await set_other_led(ws,0)
        print("tx:", await ws.recv())

        await asyncio.sleep(1)
        
        print("Poll other node...")
        await ws.send('{"type":"can.tx","bus":0,"id":291,"ext":false,"rtr":true,"dlc":1,"data":[]}')
        while True:
            js = await ws.recv()
            res = json.loads(js)
            if res["type"] == "can.rx":
                print("rx:", res)
                break

parser = argparse.ArgumentParser()
parser.add_argument("--host", required=True, help="UID-suffixed hostname or IP address")
args = parser.parse_args()
asyncio.run(main(f"ws://{args.host}/can"))
