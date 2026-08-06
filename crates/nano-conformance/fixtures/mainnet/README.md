# mainnet fixtures

Five consecutive blocks and the reward set published for the cycle they fall
in, captured from `api.mainnet.hiro.so` running stacks-node 4.0.1 at burn
height 960,300 — after the epoch 4.0 boundary at 960,230.

| file | source |
|---|---|
| `blocks/<block_id>.bin` | `GET /v3/blocks/:block_id`, consensus-serialized |
| `checkpoint-block.bin` | the same call for `a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d` |
| `stacker_set-140.json` | `GET /v3/stacker_set/140`, trimmed to signing keys and weights |

`checkpoint-block.bin` is the block that sealed the mainnet checkpoint nano runs
from — Stacks height 8,665,600, burn height 960,219, the same reward cycle as the
five above. It is here because the trust root is a claim about *that* height: the
checkpoint publishes a state root, and this header is where a reward set signed
it. Without it the attestation could only be exercised against a later block
standing in for a checkpoint.

Neither needs a chainstate, which is what makes this possible where a full
replay of mainnet is not: the reward set is published and the envelope is
self-contained.

`mainnet_envelope.rs` checks that nano derives the same signer signature hash
mainnet signed, recovers the same keys from it, orders them the same way, and
counts the same weight against the same threshold. It says nothing about
execution — see [[037-replay-mainnet-from-the-epoch-4-0-boundary]] for what
still needs a synced node.

To refresh, or to check a wider range than the five kept here:

```sh
curl -s "https://api.mainnet.hiro.so/extended/v2/blocks?limit=20" |
  jq -r '.results[].index_block_hash | ltrimstr("0x")' |
  while read -r id; do
    curl -s "https://api.mainnet.hiro.so/v3/blocks/$id" -o "$id.bin"
    cargo xtask verify-block "$id.bin" stacker_set-140.json
  done
```
