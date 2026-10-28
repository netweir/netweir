"""netweir.llm: what's sent to the model and how its answer comes back,
with a stand-in for the API, so nothing here calls a model."""

import asyncio
import json
import sys
import types

import pytest

anthropic = pytest.importorskip("anthropic")
pydantic = pytest.importorskip("pydantic")

import netweir  # noqa: E402
from netweir import llm  # noqa: E402
from netweir._native import _response  # noqa: E402

PAGE = """<html><head><title>Sourdough loaf</title></head><body>
<nav><a href="/">Home</a> <a href="/shop">Shop</a></nav>
<main><h1>Sourdough loaf</h1>
<p>A slow-proved white loaf with a dark crust, baked every morning in our Leeds bakery
from flour, water and salt, and nothing else. Each loaf weighs about 800 g.</p>
<p>Ingredients: flour, water, salt. Keeps for three days in a bread bin.</p></main>
<footer>Crumb Ltd</footer></body></html>"""


def page(html=PAGE, url="https://bakery.example/sourdough"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


def reply(text=None, stop="end_turn", details=None):
    content = [] if text is None else [types.SimpleNamespace(type="text", text=text)]
    return types.SimpleNamespace(stop_reason=stop, stop_details=details, content=content)


class Fake:
    """Stands in for an Anthropic client: keeps each request, answers with
    ``answer``."""

    def __init__(self, answer):
        self.sent = []
        outer = self

        class Messages:
            def create(self, **request):
                outer.sent.append(request)
                return answer

        self.beta = types.SimpleNamespace(messages=Messages())


def test_described_fields_come_back_as_a_dict():
    client = Fake(reply(json.dumps({"name": "Sourdough loaf", "weight": "about 800 g"})))
    got = llm.extract(page(), {"name": "the loaf's name", "weight": "its weight"}, client=client)
    assert got == {"name": "Sourdough loaf", "weight": "about 800 g"}
    request = client.sent[0]
    assert request["model"] == "claude-opus-5-5"
    schema = request["output_config"]["format"]["schema"]
    assert schema["required"] == ["name", "weight"]
    assert schema["properties"]["weight"] == {
        "type": ["string", "null"],
        "description": "its weight",
    }
    assert schema["additionalProperties"] is False
    assert request["output_config"]["effort"] == "medium"
    text = request["messages"][0]["content"]
    assert "URL: https://bakery.example/sourdough" in text and "Title: Sourdough loaf" in text
    assert "# Sourdough loaf" in text and "800 g" in text
    assert "Crumb Ltd" not in text, "the main content only"
    assert "fallbacks" not in request, "only the Claude API's own client takes them"


def test_a_pydantic_model_comes_back_validated():
    class Loaf(pydantic.BaseModel):
        name: str
        ingredients: list[str]
        keeps_days: int | None

    client = Fake(
        reply(json.dumps({"name": "Sourdough", "ingredients": ["flour"], "keeps_days": 3}))
    )
    got = llm.extract(page(), Loaf, client=client)
    assert isinstance(got, Loaf) and got.keeps_days == 3
    assert "ingredients" in client.sent[0]["output_config"]["format"]["schema"]["properties"]


def test_a_json_schema_is_sent_as_given():
    schema = {"type": "object", "properties": {"price": {"type": "number"}}, "required": ["price"]}
    client = Fake(reply('{"price": 4.5}'))
    assert llm.extract(page(), schema, client=client) == {"price": 4.5}
    assert client.sent[0]["output_config"]["format"]["schema"]["properties"] == {
        "price": {"type": "number"}
    }


def test_options_reach_the_request():
    client = Fake(reply('{"name": null}'))
    llm.extract(
        page(),
        {"name": "the name"},
        client=client,
        model="claude-sonnet-5-5",
        effort="low",
        main_only=False,
        max_chars=30,
        instructions="Prices in pence.",
    )
    request = client.sent[0]
    assert (request["model"], request["output_config"]["effort"]) == ("claude-sonnet-5-5", "low")
    text = request["messages"][0]["content"]
    assert text.endswith("Prices in pence.")
    body = text.split("<page>\n")[1].split("\n</page>")[0]
    assert len(body) == 30 and "Home" in body, "the whole page, cut"


def test_the_claude_apis_own_client_falls_back_on_a_refusal(monkeypatch):
    client = anthropic.Anthropic(api_key="test")
    sent = []
    monkeypatch.setattr(
        client.beta.messages, "create", lambda **r: sent.append(r) or reply('{"name": "x"}')
    )
    llm.extract(page(), {"name": "the name"}, client=client)
    assert sent[0]["fallbacks"] == "default"
    assert sent[0]["betas"] == ["server-side-fallback-2026-07-01"]


def test_a_refusal_a_cut_off_and_no_answer_raise():
    details = types.SimpleNamespace(category="cyber", explanation="not this")
    with pytest.raises(llm.Refused, match="cyber") as refused:
        llm.extract(page(), {"a": "b"}, client=Fake(reply(stop="refusal", details=details)))
    assert refused.value.explanation == "not this"
    with pytest.raises(llm.Error, match="cut off"):
        llm.extract(page(), {"a": "b"}, client=Fake(reply('{"a": "', stop="max_tokens")))
    with pytest.raises(llm.Error, match="no answer"):
        llm.extract(page(), {"a": "b"}, client=Fake(reply()))


def test_bad_schemas_and_sizes_are_refused_before_any_call():
    client = Fake(reply("{}"))
    for bad in ({}, ["name"], {"name": 3}):
        with pytest.raises(TypeError):
            llm.extract(page(), bad, client=client)
    with pytest.raises(ValueError):
        llm.extract(page(), {"a": "b"}, client=client, max_chars=0)
    assert client.sent == []


def test_async_extraction():
    class AsyncFake:
        def __init__(self):
            self.sent = []
            outer = self

            class Messages:
                async def create(self, **request):
                    outer.sent.append(request)
                    return reply('{"name": "Sourdough loaf"}')

            self.beta = types.SimpleNamespace(messages=Messages())

    client = AsyncFake()
    got = asyncio.run(llm.extract_async(page(), {"name": "the name"}, client=client))
    assert got == {"name": "Sourdough loaf"}


def test_without_the_sdk_it_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "anthropic", None)
    with pytest.raises(ImportError, match=r"netweir\[llm\]"):
        llm.extract(page(), {"a": "b"})


@pytest.mark.skipif(
    not __import__("os").environ.get("NETWEIR_LLM_LIVE"),
    reason="calls the Claude API: set NETWEIR_LLM_LIVE=1 with credentials to run",
)
def test_a_real_model_reads_the_page():
    got = llm.extract(page(), {"name": "the loaf's name", "weight": "its weight"}, effort="low")
    assert "sourdough" in (got["name"] or "").lower()
    assert "800" in (got["weight"] or "")
