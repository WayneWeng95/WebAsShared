"""MediaReview on Apache Flink Stateful Functions.

The RTSFaaS MediaReview app (login READ user_pwd / rate WRITE movie_rating /
review WRITE movie_review) re-expressed in the StateFun programming model: one
stateful function keyed by the row id, holding the three tables as a single
per-key `State`. Each event is a single-key access, so there is no cross-
partition fan-out here (contrast socialnetwork_function).

Wire-compatible with the vendored statefun-python-ycsb-example: consumes
`Wrapper{request_id, Any}` from Kafka and emits `Response` to the `responses`
egress, so the same evaluation/ harness drives it. Build mirrors
ycsb-example/original/docker/Dockerfile.account.
"""
from statefun import StatefulFunctions
from statefun import AsyncRequestReplyHandler
from statefun import kafka_egress_record

import logging

from google.protobuf.any_pb2 import Any

from messages_pb2 import Wrapper, Response, MRState
from messages_pb2 import MRSeed, MRLogin, MRRate, MRReview

FORMAT = '[%(asctime)s] %(levelname)-8s %(message)s'
logging.basicConfig(level=logging.INFO, format=FORMAT)
logger = logging.getLogger()

functions = StatefulFunctions()


@functions.bind("streaming-example/mediareview_function")
async def mediareview_function(context, request: Wrapper):
    state = context.state('state').unpack(MRState)
    request_id = request.request_id
    message = request.message

    if message.Is(MRSeed.DESCRIPTOR):
        seed = MRSeed()
        message.Unpack(seed)
        await handle_seed(context, state, seed, request_id)
    elif message.Is(MRLogin.DESCRIPTOR):
        login = MRLogin()
        message.Unpack(login)
        await handle_login(context, state, login, request_id)
    elif message.Is(MRRate.DESCRIPTOR):
        rate = MRRate()
        message.Unpack(rate)
        await handle_rate(context, state, rate, request_id)
    elif message.Is(MRReview.DESCRIPTOR):
        review = MRReview()
        message.Unpack(review)
        await handle_review(context, state, review, request_id)
    else:
        await send_response(context, request_id, 'unknown', 500)


async def handle_seed(context, state, message: MRSeed, request_id: str):
    # Seed phase: populate user_pwd.password for this key (rate/review start empty).
    if not state:
        state = MRState()
    state.password = message.password
    context.state('state').pack(state)
    await send_response(context, request_id, 'seed', 200)


async def handle_login(context, state, message: MRLogin, request_id: str):
    # login READ user_pwd.password — the correctness gate (200 == match).
    if state and state.password == message.password:
        await send_response(context, request_id, 'login', 200)
    else:
        await send_response(context, request_id, 'login', 422)


async def handle_rate(context, state, message: MRRate, request_id: str):
    # rate WRITE movie_rating.rate
    if not state:
        state = MRState()
    state.rate = message.rate
    context.state('state').pack(state)
    await send_response(context, request_id, 'rate', 200)


async def handle_review(context, state, message: MRReview, request_id: str):
    # review WRITE movie_review.review
    if not state:
        state = MRState()
    state.review = message.review
    context.state('state').pack(state)
    await send_response(context, request_id, 'review', 200)


async def send_response(context, request_id: str, op: str, status_code: int):
    response = Response(request_id=request_id, status_code=status_code, op=op)
    egress_message = kafka_egress_record(topic="responses", key=request_id, value=response)
    context.pack_and_send_egress("streaming-example/kafka-egress", egress_message)


from aiohttp import web

handler = AsyncRequestReplyHandler(functions)


async def handle(request):
    req = await request.read()
    res = await handler(req)
    return web.Response(body=res, content_type="application/octet-stream")

app = web.Application()
app.add_routes([web.post('/statefun', handle)])

if __name__ == '__main__':
    web.run_app(app, port=80)
