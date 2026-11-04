# Fixtures

Trimmed `inputSchema` objects from real MCP servers, plus two
hand-written stand-ins, so the mapping matrix runs against real shapes.
All upstream work is MIT-licensed.

## Vendored from github/github-mcp-server

Source: <https://github.com/github/github-mcp-server>, MIT license,
`main @ 4bcdcdf8d7be`, fetched 2026-11-03. The files carry the
`inputSchema` object of the named tool, verbatim:

- `create_pull_request.json` — flat scalars, optional booleans, an
  array of strings.
- `search_repositories.json` — a boolean with `default: true`
  (`minimal_output`), string `enum`s (`sort`, `order`), constraint
  keywords (`minimum`, `maximum`) the mapping ignores.
- `update_pull_request.json` — the `state: ["open", "closed"]` enum.
- `create_issue.json` — flat required scalars, with `x-mcp-header`
  extension keys the mapping ignores.
- `projects_write_excerpt.json` — hand-trimmed from the 26-property
  `projects_write` tool: `iterations` (array of objects with per-item
  `required`) and `updated_field` (a `oneOf` of object variants) kept
  verbatim, every other property dropped, `required` emptied because
  both listed names (`method`, `owner`) were among the dropped.

## Hand-written stand-ins

Documented stand-ins for the other two servers named in the spec; the
shapes are trimmed from their actual tool definitions, not invented.

- `additional_fields.json` — the pydantic shape `sooperset/mcp-atlassian`
  emits: required scalars, a `pattern` the mapping ignores, and
  `additional_fields` as a three-variant `anyOf` (`object | string |
  null`), which is the JSON-flag case, not the nullable case.
- `get_scene_info.json` — the FastMCP shape `ahujasid/blender-mcp`
  emits for every optional parameter: `anyOf: [<T>, null]` nullable
  forms (`user_prompt` with `default: ""`, `query`, `fields` as a
  nullable array of strings, `limit` integer with `default: 20`) and a
  required `code` string.
