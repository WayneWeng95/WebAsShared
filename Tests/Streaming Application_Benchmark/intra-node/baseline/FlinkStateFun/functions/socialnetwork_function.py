"""SocialNetwork on Apache Flink Stateful Functions.

The RTSFaaS SocialNetwork app re-expressed in the StateFun model. One stateful
function keyed by row id holds user_pwd / user_profile / tweet as a per-key
`State`:

    userLogin   READ  user_pwd       (single key)
    userProfile READ  user_profile   (single key)
    getTimeLine READ  tweet  x2 keys (key and key+1)
    postTweet   WRITE tweet  x2 keys (key and key+1)

The x2-key ops (timeline/post) fan out to the key+1 partition *inside the
dataflow* via context.pack_and_send — a cross-partition state access. In
StateFun that second access is a message hop between (possibly remote) keyed
partitions; that hop is exactly the cross-node state-transfer cost WebAsShared
removes with its zero-copy SHM page-chain (intra-node) / serialization-free RDMA
(inter-node). Each accessed key emits its own Response, so the measure tallies
two timeline-reads per getTimeLine and two tweet-writes per postTweet, matching
the WebAsShared correctness gate.

Wire-compatible with statefun-python-ycsb-example (Wrapper in / Response out
over Kafka). The Union annotation lets this one function also receive the
internal fan-out messages (SNTimeline2 / SNPost2).
"""
from statefun import StatefulFunctions
from statefun import AsyncRequestReplyHandler
from statefun import kafka_egress_record

import logging
import typing

from messages_pb2 import Wrapper, Response, SNState
from messages_pb2 import SNSeed, SNLogin, SNProfile, SNTimeline, SNPost
from messages_pb2 import SNTimeline2, SNPost2

FORMAT = '[%(asctime)s] %(levelname)-8s %(message)s'
logging.basicConfig(level=logging.INFO, format=FORMAT)
logger = logging.getLogger()

FUNCTION_TYPE = "streaming-example/socialnetwork_function"

functions = StatefulFunctions()


@functions.bind(FUNCTION_TYPE)
async def socialnetwork_function(
        context,
        message: typing.Union[Wrapper, SNTimeline2, SNPost2]):
    # Internal fan-out messages (the second key of a 2-key op) arrive unwrapped.
    if isinstance(message, SNTimeline2):
        await handle_timeline_second(context, message)
        return
    if isinstance(message, SNPost2):
        await handle_post_second(context, message)
        return

    # Otherwise it is a Wrapper from the Kafka ingress.
    state = context.state('state').unpack(SNState)
    request_id = message.request_id
    payload = message.message

    if payload.Is(SNSeed.DESCRIPTOR):
        seed = SNSeed()
        payload.Unpack(seed)
        await handle_seed(context, state, seed, request_id)
    elif payload.Is(SNLogin.DESCRIPTOR):
        login = SNLogin()
        payload.Unpack(login)
        await handle_login(context, state, login, request_id)
    elif payload.Is(SNProfile.DESCRIPTOR):
        profile = SNProfile()
        payload.Unpack(profile)
        await handle_profile(context, state, profile, request_id)
    elif payload.Is(SNTimeline.DESCRIPTOR):
        timeline = SNTimeline()
        payload.Unpack(timeline)
        await handle_timeline(context, state, timeline, request_id)
    elif payload.Is(SNPost.DESCRIPTOR):
        post = SNPost()
        payload.Unpack(post)
        await handle_post(context, state, post, request_id)
    else:
        await send_response(context, request_id, 'unknown', 500)


def next_key(context):
    """The +1 key this op also touches (the cross-partition access).

    SdkAddress exposes the function id as `.identity`.
    """
    return str(int(context.address.identity) + 1)


async def handle_seed(context, state, message: SNSeed, request_id: str):
    if not state:
        state = SNState()
    state.password = message.password
    state.profile = message.profile
    state.tweet = message.tweet
    context.state('state').pack(state)
    await send_response(context, request_id, 'seed', 200)


async def handle_login(context, state, message: SNLogin, request_id: str):
    if state and state.password == message.password:
        await send_response(context, request_id, 'login', 200)
    else:
        await send_response(context, request_id, 'login', 422)


async def handle_profile(context, state, message: SNProfile, request_id: str):
    if state and state.profile:
        await send_response(context, request_id, 'profile', 200)
    else:
        await send_response(context, request_id, 'profile', 422)


async def handle_timeline(context, state, message: SNTimeline, request_id: str):
    # Read this key's tweet, then fan out to key+1 for the second read.
    status = 200 if (state and state.tweet) else 422
    await send_response(context, request_id, 'timeline', status)
    context.pack_and_send(FUNCTION_TYPE, next_key(context),
                          SNTimeline2(request_id=request_id))


async def handle_timeline_second(context, message: SNTimeline2):
    state = context.state('state').unpack(SNState)
    status = 200 if (state and state.tweet) else 422
    # Suffix the request_id so this second access is matched distinctly.
    await send_response(context, message.request_id + '#2', 'timeline', status)


async def handle_post(context, state, message: SNPost, request_id: str):
    # Write this key's tweet, then fan out to key+1 for the second write.
    if not state:
        state = SNState()
    state.tweet = message.tweet
    context.state('state').pack(state)
    await send_response(context, request_id, 'post', 200)
    context.pack_and_send(FUNCTION_TYPE, next_key(context),
                          SNPost2(request_id=request_id, tweet=message.tweet))


async def handle_post_second(context, message: SNPost2):
    state = context.state('state').unpack(SNState)
    if not state:
        state = SNState()
    state.tweet = message.tweet
    context.state('state').pack(state)
    await send_response(context, message.request_id + '#2', 'post', 200)


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
