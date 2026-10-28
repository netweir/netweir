# Fields a language model reads

[Auto-extraction](extraction.md) reads products, jobs, articles, places,
listings and more without selectors, and costs nothing. For a page that
fits no type, such as a recipe, a council's bin collection days or a
festival's line-up, describe the fields you want and let Claude read them:

```python
import netweir
from netweir import llm

page = netweir.get("https://bakery.example/sourdough")
llm.extract(page, {
    "name": "the loaf's name",
    "weight": "its weight, with the unit",
    "keeps": "how long it keeps",
})
# {'name': 'Sourdough loaf', 'weight': 'about 800 g', 'keeps': 'three days in a bread bin'}
```

Each field you describe comes back as a string, or None when the page
doesn't say. For other types, give a pydantic model, and you get an instance
of it back, validated:

```python
from pydantic import BaseModel

class Recipe(BaseModel):
    title: str
    serves: int | None
    ingredients: list[str]
    minutes: int | None

recipe = llm.extract(page, Recipe)
recipe.ingredients  # ['500 g strong white flour', '350 g water', '10 g salt']
```

A JSON schema works too, and gives a dict; a field it doesn't list as
required is asked for anyway, null when the page doesn't say. Whichever you
use, the API holds the model to the schema's fields and types, so there's
no free text to parse. Limits such as `minimum`, `pattern` or `maxLength`
are passed on as advice rather than enforced, and a pydantic model's own
checks run on the answer.

`netweir.llm` is new. If a call fails with an error from the API, please
[open an issue](https://github.com/netweir/netweir/issues) with the error.

## Setting it up

`netweir.llm` needs the Anthropic SDK and a Claude API key:

```
pip install 'netweir[llm]'
export ANTHROPIC_API_KEY=sk-ant-...
```

Nothing calls a model unless you call `llm.extract`. Netweir has no key or
service of its own: each call goes from your machine straight to Anthropic's
API (or your cloud platform) with your key, and is billed to your account.
That makes it for pages where auto-extraction and selectors fall short, not
for every page of a crawl.

## What it sends

The model gets the page's main content as Markdown (as `page.markdown(main_only=True)`
gives it), or the whole page when there's no main content, with the page's
URL and title. With `main_only=False` it always gets the whole page.
Nothing is cut unless you say so: `max_chars=50_000` keeps the first 50,000
characters.

| Option | Default | Does |
|---|---|---|
| `instructions=` | none | more to tell the model, such as "Prices in pence" |
| `model=` | `"claude-opus-5-5"` | the Claude model to ask |
| `effort=` | `"medium"` | how hard it thinks: `"low"` to `"max"`. A simple page may read as well at `"low"`, for less |
| `main_only=` | `True` | the main content only, or the whole page |
| `max_chars=` | none | cut the Markdown to this many characters |
| `client=` | `anthropic.Anthropic()` | the client to call with: one with its own key or settings, or a Bedrock, Vertex AI or Foundry client, with `model=` set to that platform's name for the model |

With the Claude API's own client, a request the model declines is run again
on another model in the same call. When every model declines,
`netweir.llm.Refused` says why. An answer cut off, or one a pydantic model
won't accept, raises `netweir.llm.Error`.

## In a spider

`extract_async` takes the same arguments, for callbacks:

```python
class Loaf(BaseModel):
    name: str
    weight: str | None


class Bakeries(netweir.Spider):
    start_urls = ["https://bakery.example/breads"]

    async def parse(self, page):
        for href in page.css("a.loaf::attr(href)").getall():
            yield page.follow(href, self.loaf)

    async def loaf(self, page):
        yield await llm.extract_async(page, Loaf, effort="low")
```

## What to know

- The page goes to the model as data. Netweir tells the model that text on
  the page addressing it is part of the page, not an instruction, but a page
  can still try to mislead it: check what matters.
- The model fills in only what the page states, and leaves the rest None,
  but it can misread. For fields that must be right every time, pin them
  with a selector instead.
- Other providers' models aren't supported yet.
