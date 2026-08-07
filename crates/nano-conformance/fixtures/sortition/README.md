# The sortition history this capture seeds a chain from

`snapshots.json` is captured from stacks-core's own `snapshots` table.
`consensus-hashes.json` is **derived from it**, not separately captured: it is the
`consensus_hash` of every row up to burn **459**, in burn-height order.

Two things about that number. It is the last burn block below the capture's anchor
that elected somebody — a chain is seeded by the snapshot its history ends at, and
the sampling of the block after a seed mixes the most recent winner's VRF seed, so a
seed whose own block won nothing cannot supply it. And it is *below* the anchor on
purpose: a chain only walks forward, so a history ending above the burn view
execution starts at seeds a chain that can never answer for it.

It exists because [[077-remove-peer-derived-consensus-execution-fallbacks]] removed
the path where a peer's `/v3/sortitions` answer became the burn view a block executed
under. Rigs that used to rely on that now derive their own, and a capture with no
history cannot seed one — which is what `capture-fixtures` failed to write until
`6383fc82`, and what this file stands in for until the tree is recaptured with it.

Regenerate from `snapshots.json` when the capture is replaced:

    python3 - <<'PY'
    import json
    rows = sorted(json.load(open('snapshots.json')), key=lambda r: r['block_height'])
    seed = max(r['block_height'] for r in rows
               if r.get('sortition') == 1 and r['block_height'] < ANCHOR_BURN_HEIGHT)
    json.dump({"hashes": [r['consensus_hash'] for r in rows if r['block_height'] <= seed]},
              open('consensus-hashes.json', 'w'))
    PY
