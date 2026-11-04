"""Reference Graphiti REST service for the memory-sidecar evaluation.

This is a deployment example, not a production service. It deliberately has no
authentication, authorization, or prompt-injection protection, matching the
upstream Graphiti reference server. Bind it to loopback and never expose it to
an untrusted network.

The wire shape follows `getzep/graphiti`'s `server/graph_service` (Apache-2.0),
with three additions the project's `MemorySidecar` port needs:

* `DELETE /group/{group_id}/episode/{uuid}/cleanup` calls `Graphiti.remove_episode`,
  which also repairs facts and entity summaries derived from the retired
  episode. The reference server's `DELETE /episode/{uuid}` removes only the
  episode node and therefore cannot satisfy the port's retirement contract. The
  group is in the route because FalkorDB keeps one graph per group and
  `remove_episode` is not group-scoped upstream.
* The LLM and embedder are selectable between an OpenAI-compatible endpoint
  (OpenAI, OpenRouter, Ollama, vLLM, ...) and Gemini's native client.
* A pass-through reranker is used by default. The upstream OpenAI reranker
  needs logprobs, and not every OpenAI-compatible endpoint supplies them; a
  deployment that relies on the reranker must set one explicitly. This is part
  of the measured configuration and is documented in the example README.

The episode body is written with the upstream envelope
`f'{role or ""}({role_type}): {content}'`; `GraphitiSidecar` strips exactly
that prefix when it reads a claim back.
"""

from __future__ import annotations

import asyncio
import logging
import os
from contextlib import asynccontextmanager, suppress
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Annotated, Literal

from fastapi import APIRouter, Depends, FastAPI, HTTPException, Query, Request, status
from graphiti_core import Graphiti
from graphiti_core.cross_encoder.client import CrossEncoderClient
from graphiti_core.driver.falkordb_driver import FalkorDriver
from graphiti_core.edges import EntityEdge
from graphiti_core.errors import GroupsEdgesNotFoundError, NodeNotFoundError
from graphiti_core.llm_client.config import LLMConfig
from graphiti_core.nodes import EntityNode, EpisodeType, EpisodicNode
from pydantic import BaseModel, Field

logger = logging.getLogger("aivtuber.graphiti")
logging.basicConfig(
    level=os.getenv("GRAPHITI_LOG_LEVEL", "INFO").upper(),
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)

DEFAULT_OPENAI_MODEL = "gpt-4.1-mini"
DEFAULT_GEMINI_MODEL = "gemini-2.5-flash"
DEFAULT_OPENAI_EMBEDDING = "text-embedding-3-small"
DEFAULT_GEMINI_EMBEDDING = "gemini-embedding-001"
DEFAULT_EPISODE_WINDOW = 64


def _env(name: str, default: str | None = None, *, required: bool = False) -> str:
    value = os.getenv(name, default)
    if required and not value:
        raise RuntimeError(f"{name} is required")
    return value or ""


def _env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or raw == "":
        return default
    try:
        return int(raw)
    except ValueError as error:
        raise RuntimeError(f"{name} must be an integer: {raw!r}") from error


def _env_float(name: str, default: float) -> float:
    raw = os.getenv(name)
    if raw is None or raw == "":
        return default
    try:
        return float(raw)
    except ValueError as error:
        raise RuntimeError(f"{name} must be a number: {raw!r}") from error


@dataclass(frozen=True)
class Settings:
    llm_provider: str
    llm_model: str
    llm_base_url: str
    llm_api_key: str
    llm_temperature: float
    llm_max_tokens: int
    structured_output_mode: str
    embedder_provider: str
    embedder_model: str
    embedder_base_url: str
    embedder_api_key: str
    embedder_dim: int
    falkordb_host: str
    falkordb_port: int
    falkordb_database: str

    @classmethod
    def from_env(cls) -> "Settings":
        llm_provider = _env("LLM_PROVIDER", "openai").lower()
        if llm_provider not in {"openai", "gemini"}:
            raise RuntimeError("LLM_PROVIDER must be 'openai' or 'gemini'")

        if llm_provider == "gemini":
            llm_api_key = _env("LLM_API_KEY") or _env("GEMINI_API_KEY") or _env("GOOGLE_API_KEY")
            llm_model = _env("LLM_MODEL", DEFAULT_GEMINI_MODEL)
            llm_base_url = ""
        else:
            llm_api_key = _env("LLM_API_KEY") or _env("OPENAI_API_KEY")
            llm_model = _env("LLM_MODEL", DEFAULT_OPENAI_MODEL)
            llm_base_url = _env("LLM_BASE_URL", "https://api.openai.com/v1")
        if not llm_api_key:
            raise RuntimeError(
                "the selected LLM provider needs LLM_API_KEY (or OPENAI_API_KEY / "
                "GEMINI_API_KEY / GOOGLE_API_KEY)"
            )

        embedder_provider = _env("EMBEDDER_PROVIDER", llm_provider).lower()
        if embedder_provider not in {"openai", "gemini"}:
            raise RuntimeError("EMBEDDER_PROVIDER must be 'openai' or 'gemini'")
        if embedder_provider == "gemini":
            embedder_api_key = (
                _env("EMBEDDER_API_KEY") or _env("GEMINI_API_KEY") or _env("GOOGLE_API_KEY")
            )
            embedder_model = _env("EMBEDDER_MODEL", DEFAULT_GEMINI_EMBEDDING)
            embedder_base_url = ""
            default_dim = 1024
        else:
            embedder_api_key = _env("EMBEDDER_API_KEY") or llm_api_key
            embedder_model = _env("EMBEDDER_MODEL", DEFAULT_OPENAI_EMBEDDING)
            embedder_base_url = _env("EMBEDDER_BASE_URL", llm_base_url)
            default_dim = 1536
        if not embedder_api_key:
            raise RuntimeError(
                "the selected embedder provider needs EMBEDDER_API_KEY or the LLM key"
            )

        structured_output_mode = _env("LLM_STRUCTURED_OUTPUT", "json_schema").lower()
        if structured_output_mode not in {"json_schema", "json_object"}:
            raise RuntimeError("LLM_STRUCTURED_OUTPUT must be 'json_schema' or 'json_object'")

        return cls(
            llm_provider=llm_provider,
            llm_model=llm_model,
            llm_base_url=llm_base_url,
            llm_api_key=llm_api_key,
            llm_temperature=_env_float("LLM_TEMPERATURE", 0.0),
            llm_max_tokens=_env_int("LLM_MAX_TOKENS", 8192),
            structured_output_mode=structured_output_mode,
            embedder_provider=embedder_provider,
            embedder_model=embedder_model,
            embedder_base_url=embedder_base_url,
            embedder_api_key=embedder_api_key,
            embedder_dim=_env_int("EMBEDDER_DIM", default_dim),
            falkordb_host=_env("FALKORDB_HOST", "127.0.0.1"),
            falkordb_port=_env_int("FALKORDB_PORT", 6379),
            falkordb_database=_env("FALKORDB_DATABASE", "default_db"),
        )


class PassThroughReranker(CrossEncoderClient):
    """Keep Graphiti's hybrid-search order without a provider reranker.

    Graphiti's upstream OpenAI reranker asks for logprobs, which some
    OpenAI-compatible endpoints do not implement. A pass-through ranker keeps
    the deployment runnable and makes the measured configuration explicit:
    retrieval order is Graphiti's hybrid search order, not a reranked one.
    """

    async def rank(self, query: str, passages: list[str]) -> list[tuple[str, float]]:
        del query
        return [(passage, 1.0) for passage in passages]


def _build_llm_client(settings: Settings):
    if settings.llm_provider == "gemini":
        from graphiti_core.llm_client.gemini_client import GeminiClient

        return GeminiClient(
            config=LLMConfig(
                api_key=settings.llm_api_key,
                model=settings.llm_model,
                temperature=settings.llm_temperature,
                max_tokens=settings.llm_max_tokens,
            ),
            max_tokens=settings.llm_max_tokens,
        )

    from graphiti_core.llm_client.openai_generic_client import OpenAIGenericClient

    return OpenAIGenericClient(
        config=LLMConfig(
            api_key=settings.llm_api_key,
            model=settings.llm_model,
            base_url=settings.llm_base_url,
            temperature=settings.llm_temperature,
            max_tokens=settings.llm_max_tokens,
        ),
        max_tokens=settings.llm_max_tokens,
        structured_output_mode=settings.structured_output_mode,
    )


def _build_embedder(settings: Settings):
    if settings.embedder_provider == "gemini":
        from graphiti_core.embedder.gemini import GeminiEmbedder, GeminiEmbedderConfig

        return GeminiEmbedder(
            config=GeminiEmbedderConfig(
                api_key=settings.embedder_api_key,
                embedding_model=settings.embedder_model,
                embedding_dim=settings.embedder_dim,
            )
        )

    from graphiti_core.embedder.openai import OpenAIEmbedder, OpenAIEmbedderConfig

    return OpenAIEmbedder(
        config=OpenAIEmbedderConfig(
            api_key=settings.embedder_api_key,
            base_url=settings.embedder_base_url,
            embedding_model=settings.embedder_model,
            embedding_dim=settings.embedder_dim,
        )
    )


def build_graphiti(settings: Settings) -> Graphiti:
    driver = FalkorDriver(
        host=settings.falkordb_host,
        port=settings.falkordb_port,
        database=settings.falkordb_database,
    )
    return Graphiti(
        graph_driver=driver,
        llm_client=_build_llm_client(settings),
        embedder=_build_embedder(settings),
        cross_encoder=PassThroughReranker(),
    )


class Result(BaseModel):
    message: str
    success: bool


class Message(BaseModel):
    content: str = Field(..., description="The content of the message")
    uuid: str | None = Field(default=None, description="The episode uuid (optional)")
    name: str = Field(default="", description="The name of the episodic node")
    role_type: Literal["user", "assistant", "system"] = Field(
        ..., description="The role type of the message"
    )
    role: str | None = Field(
        default="",
        description="The custom role used alongside role_type; empty means absent",
    )
    timestamp: datetime = Field(
        default_factory=lambda: datetime.now(timezone.utc),
        description="The event time of the message",
    )
    source_description: str = Field(
        default="", description="The source description carried by the episode"
    )


class AddMessagesRequest(BaseModel):
    group_id: str = Field(..., description="The group id of the messages to add")
    messages: list[Message] = Field(..., description="The messages to add")


class SearchQuery(BaseModel):
    group_ids: list[str] | None = Field(
        default=None, description="The group ids for the memories to search"
    )
    query: str
    max_facts: int = Field(default=10, ge=1, le=256)


class FactResult(BaseModel):
    uuid: str
    name: str
    fact: str
    valid_at: datetime | None
    invalid_at: datetime | None
    created_at: datetime
    expired_at: datetime | None
    source_node_uuid: str
    target_node_uuid: str
    episodes: list[str] = Field(default_factory=list)


class SearchResults(BaseModel):
    facts: list[FactResult]


class AsyncWorker:
    """Process episodes one at a time, as Graphiti requires for a scope."""

    def __init__(self) -> None:
        self.queue: asyncio.Queue[object] = asyncio.Queue()
        self._task: asyncio.Task[None] | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._run())

    async def stop(self) -> None:
        if self._task is None:
            return
        self._task.cancel()
        with suppress(asyncio.CancelledError):
            await self._task
        self._task = None

    async def _run(self) -> None:
        while True:
            job = await self.queue.get()
            try:
                await job()  # type: ignore[operator]
            except asyncio.CancelledError:
                raise
            except Exception:
                # The adapter polls the episode list and reports the timeout;
                # the server keeps the queue alive so one bad claim does not
                # stop the rest of the run.
                logger.exception("graphiti episode processing failed")
            finally:
                self.queue.task_done()

    def enqueue(self, job: object) -> None:
        self.queue.put_nowait(job)


async def _add_episode(graphiti: Graphiti, group_id: str, message: Message) -> None:
    await graphiti.add_episode(
        uuid=message.uuid,
        group_id=group_id,
        name=message.name,
        episode_body=f'{message.role or ""}({message.role_type}): {message.content}',
        reference_time=message.timestamp,
        source=EpisodeType.message,
        source_description=message.source_description,
    )


async def _delete_group(graphiti: Graphiti, group_id: str) -> None:
    driver = graphiti.driver
    try:
        edges = await EntityEdge.get_by_group_ids(driver, [group_id])
    except GroupsEdgesNotFoundError:
        # A scope that never materialized has no edges; opening it must still
        # succeed, and closing it must be idempotent.
        edges = []
    nodes = await EntityNode.get_by_group_ids(driver, [group_id])
    episodes = await EpisodicNode.get_by_group_ids(driver, [group_id])
    for edge in edges:
        await edge.delete(driver)
    for node in nodes:
        await node.delete(driver)
    for episode in episodes:
        await episode.delete(driver)


@asynccontextmanager
async def lifespan(app: FastAPI):
    settings = Settings.from_env()
    graphiti = build_graphiti(settings)
    app.state.settings = settings
    app.state.graphiti = graphiti
    await graphiti.build_indices_and_constraints()
    worker = AsyncWorker()
    await worker.start()
    app.state.worker = worker
    logger.info(
        "graphiti ready: llm=%s/%s embedder=%s/%s dim=%d reranker=pass-through",
        settings.llm_provider,
        settings.llm_model,
        settings.embedder_provider,
        settings.embedder_model,
        settings.embedder_dim,
    )
    try:
        yield
    finally:
        await worker.stop()
        await graphiti.close()


app = FastAPI(lifespan=lifespan)
router = APIRouter()


def get_graphiti(request: Request) -> Graphiti:
    return request.app.state.graphiti


def get_worker(request: Request) -> AsyncWorker:
    return request.app.state.worker


@router.post("/messages", status_code=status.HTTP_202_ACCEPTED)
async def add_messages(
    request: AddMessagesRequest,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
    worker: Annotated[AsyncWorker, Depends(get_worker)],
) -> Result:
    for message in request.messages:
        worker.enqueue(lambda message=message: _add_episode(graphiti, request.group_id, message))
    return Result(message="Messages added to processing queue", success=True)


@router.post("/search", status_code=status.HTTP_200_OK)
async def search(
    query: SearchQuery,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
) -> SearchResults:
    edges = await graphiti.search(
        query=query.query,
        group_ids=query.group_ids,
        num_results=query.max_facts,
    )
    facts = [
        FactResult(
            uuid=edge.uuid,
            name=edge.name,
            fact=edge.fact,
            valid_at=edge.valid_at,
            invalid_at=edge.invalid_at,
            created_at=edge.created_at,
            expired_at=edge.expired_at,
            source_node_uuid=edge.source_node_uuid,
            target_node_uuid=edge.target_node_uuid,
            episodes=edge.episodes or [],
        )
        for edge in edges
    ]
    return SearchResults(facts=facts)


@router.get("/episodes/{group_id}", status_code=status.HTTP_200_OK)
async def get_episodes(
    group_id: str,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
    last_n: int = Query(default=DEFAULT_EPISODE_WINDOW, ge=1, le=256),
) -> list[EpisodicNode]:
    return await graphiti.retrieve_episodes(
        group_ids=[group_id],
        last_n=last_n,
        reference_time=datetime.now(timezone.utc),
    )


@router.delete("/group/{group_id}", status_code=status.HTTP_200_OK)
async def delete_group(
    group_id: str,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
) -> Result:
    await _delete_group(graphiti, group_id)
    return Result(message="Group deleted", success=True)


@router.delete("/episode/{uuid}", status_code=status.HTTP_200_OK)
async def delete_episode(
    uuid: str,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
) -> Result:
    try:
        episode = await EpisodicNode.get_by_uuid(graphiti.driver, uuid)
        await episode.delete(graphiti.driver)
    except NodeNotFoundError as error:
        raise HTTPException(status_code=404, detail=str(error)) from error
    return Result(message="Episode deleted", success=True)


@router.delete("/group/{group_id}/episode/{uuid}/cleanup", status_code=status.HTTP_200_OK)
async def cleanup_episode(
    group_id: str,
    uuid: str,
    graphiti: Annotated[Graphiti, Depends(get_graphiti)],
) -> Result:
    """Retire an episode and everything derived from it.

    The group is part of the route because FalkorDB keeps one graph per group
    and `Graphiti.remove_episode` looks the episode up through the driver's
    default graph. On the unscoped route every retirement therefore raised
    `NodeNotFoundError` on a live episode, which this route reported as
    "already absent": the episode and its facts stayed in the group's graph
    while the project believed it had retired them. Scoping the driver to the
    group lets upstream removal run against the graph the episode lives in.

    A missing episode is already in the state retirement asks for, so the route
    confirms it instead of reporting a failure. The adapter calls this only
    after it received the episode handle from a successful retain.
    """
    if group_id not in await graphiti.driver.client.list_graphs():
        return Result(message="Episode already absent", success=True)
    scoped_driver = graphiti.driver.clone(database=group_id)
    scoped = Graphiti(
        graph_driver=scoped_driver,
        llm_client=graphiti.llm_client,
        embedder=graphiti.embedder,
        cross_encoder=graphiti.cross_encoder,
    )
    try:
        await scoped.remove_episode(uuid)
    except NodeNotFoundError:
        logger.warning("cleanup requested for an episode that is already gone: %s", uuid)
        return Result(message="Episode already absent", success=True)
    return Result(message="Episode and derived state removed", success=True)


app.include_router(router)


@app.get("/healthcheck", status_code=status.HTTP_200_OK)
async def healthcheck() -> dict[str, str]:
    return {"status": "healthy"}
