# Vendored crossterm

The TUI leans on two keyboard-parsing patches the stock crossterm
release does not carry (see `[patch.crates-io]` in the root Cargo.toml
for what and why):

- xterm modifyOtherKeys parsing (CSI 27;…;…~) — Ghostty's default
  shift+enter sequence;
- kitty associated text (the `;text` tail of CSI u) decoded into the key
  code — without it every AltGr character and dead key is lost.

`crossterm/` is the `modify-other-keys` branch of
`~/Documents/contrib/crossterm` archived verbatim (`git archive`):
upstream crossterm at master `3e55b57` plus exactly those two commits.
Their delta is recorded in `crossterm.patch`. The tree is deliberately
not the `0.29` release tag: upstream sits 48 commits past it with the
event parser mid-restructure, and porting the patches onto the release
would ship keyboard handling nobody has run. Upstream 0.29.0 also ships
`REPORT_ASSOCIATED_TEXT` commented out ("not yet supported"), so the
workspace cannot compile against the stock release at all — the patch
path keeps every build context (dev, CI, image) on the same parser.

## Refreshing

In the fork checkout, rebase the branch onto the new upstream base and
re-archive (paths abbreviated):

```sh
git -C ~/Documents/contrib/crossterm \
    rebase --onto <new-base> 3e55b57 modify-other-keys
git -C ~/Documents/contrib/crossterm archive modify-other-keys \
    | tar -x -C vendor/crossterm
git -C ~/Documents/contrib/crossterm diff <new-base>..modify-other-keys \
    > vendor/crossterm.patch
```

The version in `crossterm/Cargo.toml` must satisfy the `crossterm`
requirement in `crates/rhema-code/Cargo.toml`, and ratatui must still
build against it (see the dependency comment there).

## Verifying a refresh

Applying `crossterm.patch` to a pristine base must reproduce the tree:

```sh
tmp=$(mktemp -d)
git -C ~/Documents/contrib/crossterm archive <base> | tar -x -C "$tmp"
patch -d "$tmp" -p1 < vendor/crossterm.patch
diff -r "$tmp" vendor/crossterm
```
