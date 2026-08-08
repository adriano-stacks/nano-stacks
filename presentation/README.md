# nano-stacks presentation

Open `index.html` in a browser. Use the arrow keys, Page Up, Page Down, or the
space bar. Press `F` for full screen.

For a local web server:

```sh
nix shell nixpkgs#python3 -c python -m http.server 8080 -d presentation
```

Then open `http://127.0.0.1:8080`.

Print from the browser to produce a 16:9 PDF. Enable background graphics.

For a headless PDF:

```sh
nix run nixpkgs#chromium -- \
  --headless --disable-gpu --no-sandbox --no-pdf-header-footer \
  --print-to-pdf=nano-stacks-presentation.pdf \
  "file://$PWD/presentation/index.html"
```

The measurement tools are in `tools/`. See `evidence.md` for scope, commands,
commit identifiers, transcript rules, and known limits.

Run the pinned code and token measurements:

```sh
loc_python=$(nix build --no-link --print-out-paths --impure --expr \
  'import ./presentation/tools/loc.nix' | tail -1)
"$loc_python/bin/python" presentation/tools/measure_loc.py
python3 presentation/tools/session_metrics.py
```

Run the offline navigation, overflow, and print checks at three 16:9 sizes:

```sh
nix shell nixpkgs#chromium nixpkgs#poppler-utils --command \
  python3 presentation/tools/verify_deck.py
```
