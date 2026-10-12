# Rhema Code

```
curl -fsSL https://get.rhema.rs/code | sh
rhema
```

Rhema Code is a terminal-based coding agent, built on the Rhema agent framework.

<img align="center" src="../../.github/img/rhema-code.png">
<div align="center"><em>The Rhema Code TUI.</em></div>

## Features

### Rich settings

<!-- TODO Screenshot -->

### Customizable, scriptable system-prompt

```
rhema system edit
```

The above command allows you to edit the system prompt in your `$EDITOR`. It uses a Jinja template (`minijinja`), so you can customize the skills part, the tool part, etc.

### ACP (Agent Client Protocol) support

You can use Rhema Code directly in your editor or orchestrator, using the Agent Client Protocol.

```sh
rhema acp
```

An editor launches the above command and directly talks ACP with Rhema Code.

### Profiles

Rhema Code ships with two default profiles `work` and `personal` that allows you to manage your different subscription contexts. Each profile can store any amount of subscription instances.

### Skills, sessions, etc.

Regular coding agent features are supported, while maintaining a simplicity balance. Right now, no MCPs.

## Documentation

## About

```
MIT License
Copyright (c) 2026 Valentin Haudiquet
```