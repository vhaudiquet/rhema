# rhema-model-catalog

The published facts for the models the providers list: price, context
and output ceilings, vision, reasoning and tool support, release date.
The data comes from [models.dev](https://models.dev), fetched when the
first listing asks for it and cached locally.

## What it does

The adapters' `list_models` calls are the authority on what an account
can run and on the prices a route measures itself. They are not a
consistent source of facts: Anthropic's listing carries ids and display
names only, ChatGPT and Copilot carry no price, and the adapters that
do fill them each keep a hand-written table with its own fetch date.

This crate fills the blanks. `rhema-provider-registry`'s `build`
wraps every listing in a decorator, and the decorator writes a catalog
fact only where the card has nothing: an empty description, an unknown
context window, an unset vision verdict, an absent price. A value the
adapter supplied always wins.

That rule makes the adapter's values the ceiling, including a wrong
one. For vision, the adapters therefore set the fact only from what they
read: a listing that publishes modalities (OpenRouter, OpenCode,
ChatGPT, Mistral) wins, and every other adapter leaves vision `None`
so the catalog answers. A curated vision guess in an adapter would
outrank the catalog and could not be corrected; the Z.ai table had
exactly that bug and no longer carries a vision column.

Three rules are not negotiable:

- A subscription route keeps the facts and drops the per-token price.
  ChatGPT and Copilot bill a flat fee; showing the vendor's list price
  as a session cost would show a bill nobody pays.
- An unknown id stays unknown. Lookup tries the exact id, the
  case-insensitive id, a short explicit alias table, then a normalized
  slug. It never guesses across model families.
- A row missing from the provider's own block falls back to the
  gateway block (`opencode`) for facts, never for a price: a resale
  rate is not the first-party bill.

## The fetch

`https://models.dev/api.json` returns the whole catalog as one
document, currently 6.9 MB. The cache is empty on a fresh install:

1. The listing fetches the document, keeps the nine provider blocks
   below, and writes them to `$XDG_CACHE_HOME/rhema/models-dev.json`
   (`~/.cache/rhema/models-dev.json` when the variable is unset).
2. For six hours, later listings read the parsed catalog from memory.
3. After six hours the request carries the stored `ETag` as
   `If-None-Match`. A 304 moves the timestamp forward; a 200 replaces
   the body and the file.

The fetch has an eight-second budget and runs alongside the provider's
`list_models`, so the two waits do not add; the first picker open can
still wait up to eight seconds for facts. A failed fetch keeps the
last catalog, stale or not, and suppresses the next attempt for five
minutes. With no catalog at all, the cards stay exactly as the adapter
returned them. A catalog failure never fails a listing.

The file is one JSON envelope. The body and its validator share it, so
a read cannot pair one fetch's `ETag` with another's content:

```json
{ "etag": "\"7bcf...\"", "fetched_at": 1764200000, "catalog": { "anthropic": { "models": { } } } }
```

A build with the `live` feature off (a host without sockets) has no
`CatalogCache` and `decorate_listing` passes the adapter's rows
through untouched.

## The provider table

| Config spelling | models.dev key | Price |
| --- | --- | --- |
| `google` | `google` | per token |
| `anthropic` | `anthropic` | per token |
| `openai` | `openai`, fallback `opencode` | per token |
| `chatgpt` | `openai`, fallback `opencode` | none |
| `copilot` | `github-copilot` | none |
| `openrouter` | `openrouter` | per token |
| `opencode` | `opencode` | per token |
| `deepseek` | `deepseek` | per token |
| `zai` | `zai` | per token |
| `mistral` | `mistral` | per token |
| `mistral-vibe` | `mistral` | per token |

The fallback answers facts for an id the first key misses. OpenAI's own
block lacks the Codex ids the gateway lists, so `gpt-5.1-codex-max` gets
its context window from the gateway row and no price.

## Fields

| models.dev field | Card use |
| --- | --- |
| `name`, `description` | filled when the listing has none |
| `cost.input`, `cost.output`, `cost.cache_read` | `Pricing` |
| `limit.input`, `limit.context` | prompt ceiling; `input` wins when present |
| `limit.output` | output ceiling |
| `modalities.input` | `supports_vision` |
| `tool_call`, `reasoning` | exposed on `Facts`, not yet on the card |
| `family`, `knowledge`, `release_date`, `open_weights`, `canonical_model_id` | exposed on `Facts` |

## Tests

`cargo test -p rhema-model-catalog` is offline: the cache tests drive a
scripted source, and the registry's integration test points the cache
at a wiremock endpoint. The one test in `tests/live.rs` is `#[ignore]`;
run it with `cargo test -p rhema-model-catalog --test live -- --ignored`
to check the live endpoint.

models.dev is community-maintained and its prices lag the vendors'. An
adapter that has measured a route (Mistral Vibe's meter, DeepSeek's
standard rate, Google's promo tier) keeps its own number and the
catalog stays out. Z.ai's table is gone: models.dev's `zai` rows match
it on price and on the context window, so the adapter now ships no
facts for those rows at all.
