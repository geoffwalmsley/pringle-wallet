# pringle-wallet

A small, single-binary Rust CLI built on [`chia-wallet-sdk`](https://crates.io/crates/chia-wallet-sdk)
that demonstrates an end-to-end flow on **Chia mainnet**:

1. Generate and locally store a BLS key, and derive a standard `xch` wallet address.
2. Discover, consolidate, and send regular XCH coins through the public
   [coinset.org](https://api.coinset.org) API.
3. Mint a singleton NFT owned by the wallet.
4. Derive and fund a `p2 singleton` address that the NFT controls.
5. Create an option contract whose *underlying* asset is that NFT, with an XCH strike price.
6. Sell (offer/take) or exercise the option.

Because the NFT is the option underlying, whoever controls the NFT controls the funds locked in
its `p2 singleton` (those coins can only be spent by co-spending the NFT singleton). Exercising the
option transfers the NFT, and therefore control of the attached `p2 singleton` funds, to the
exerciser. Creating, funding, offering, taking, exercising, and sweeping are all implemented.
**Clawback (reclaiming the underlying after expiration) is not yet supported.**

> [!WARNING]
> This tool operates on **mainnet with real XCH** and stores your private key **unencrypted** on
> disk (`pringle-key.hex`). There is no security hardening. Use a throwaway key with a small amount
> of funds. Do not use it for anything valuable.

## Install

```sh
cargo install --path .
pringle --help
```

After changing the source, reinstall the command with:

```sh
cargo install --path . --force
```

Alternatively, use `cargo run -- <arguments>` while developing. Cargo normally installs binaries
under `~/.cargo/bin`; add that directory to `PATH` if `pringle` is not found.

## Output modes

Every command accepts these global flags:

- `--json` — emit a stable machine-readable envelope (`schema_version: 1`) on stdout. Progress
  and warnings go to stderr, so stdout stays script-safe.
- `--verbose` — show full ids, puzzle hashes, and raw phases.
- `--quiet` — print only essential values (and errors).
- `--yes` — skip the interactive confirmation prompt shown before destructive mainnet actions.

Confirmation prompts only appear when stdin/stderr are interactive terminals; non-interactive
invocations (scripts, CI) run unprompted, exactly as before. Exit codes: `0` success, `1`
recoverable/user error, `3` chain/RPC failure (status unknown).

## Multiple assets

State tracks *collections* of NFTs, p2 singletons, and options. A normal `status`/`sync`
automatically derives the p2-singleton address for every tracked NFT and refreshes all unspent
coins at those addresses, including coins sent by another wallet. When only one applicable asset
exists, commands select it automatically. When several exist, pass `--launcher <id>` to choose;
the CLI lists the valid ids if you omit it. Legacy single-asset state files are migrated
automatically and atomically on first use.

## Usage

All amounts are in **mojos** (1 XCH = 1,000,000,000,000 mojos). State is kept in
`pringle-state.json` and the key in `pringle-key.hex` in the current directory (override with
`--key-file` / `--state-file`).

```sh
# 1. Create a local key (fresh state) and print the wallet address.
pringle init
pringle address

# 2. Inspect on-chain coins for the wallet.
pringle coins
pringle coin <coin-id>

# Combine all spendable regular XCH coins into one wallet coin. To instead empty the
# regular wallet, send the full spendable balance (minus the fee) to an xch address.
# Funds at p2 singleton addresses are separate and are not included.
pringle xch consolidate --fee 100000
pringle xch send-all xch1... --fee 100000

# 3. Mint an NFT owned by the wallet (fee in mojos).
pringle nft mint --fee 100000

# 4. Derive and fund the NFT's p2 singleton (the NFT must be confirmed first).
pringle p2-singleton address
pringle p2-singleton fund --amount 1000000000 --fee 100000

# Later: sweep the entire p2 singleton balance (all coins) out in one transaction.
# Defaults to the wallet's own address; pass --address to send elsewhere. The NFT is
# co-spent to authorize the sweep, so it must be wallet-controlled (not locked in an option).
# Because the singleton layer requires exactly one odd output (the recreated NFT), an odd
# remaining mojo is unavoidably donated to the fee; the sweep reports it separately.
pringle p2-singleton sweep --fee 100000
pringle p2-singleton sweep --address xch1... --fee 100000

# 5. Create an NFT-backed XCH option.
pringle option create --strike 5000000000000 --expiration 1893456000 --fee 100000

# 6a. Sell the option: write an offer, or accept one selling an option for XCH.
pringle option offer --request 250000000 --output option.offer
pringle option take option.offer --fee 100000

# A purchased option's terms (strike/expiration/creator) aren't in the offer file; `take`
# recovers them from the chain automatically. If the option wasn't confirmed yet, run:
pringle option recover

# 6b. Or, as the option owner, exercise before expiration: pay the strike, receive the NFT.
# `show-all` refreshes from the chain, prints full launcher ids, and provides copyable
# exercise commands. By default it hides expired/exercised/transferred options.
pringle option show-all
pringle option show-all --include-closed
pringle option show-all --cached       # local snapshot only
pringle option exercise --fee 100000
# When several options are open, copy the full launcher id from `show-all`:
pringle option exercise --launcher 0x... --fee 100000

# At any point, review local lifecycle state (refreshed against the chain by default).
pringle status
pringle status --cached   # local snapshot only, no network

# Reconcile local state with the chain (fixes stale coin ids after confirmations).
pringle sync
```

Option offer files use the wallet SDK's option primitive. At the time of writing, Sage does not
support NFT-backed option underlyings, so these offers must be viewed, taken, and exercised with a
compatible tool such as Pringle.

## How status and sync work

`pringle status` reconciles against the chain by default and then reports each asset with an
intuitive state — *Ready*, *Pending confirmation*, *Locked in option*, *Expired*, *Exercised*,
*Closed*, *Transferred*, *Empty*, or *Unknown* — hiding raw phases and coin ids unless
`--verbose`. It also derives missing p2-singleton tracking records and refreshes their combined
confirmed balances. Use `--cached` for a no-network snapshot, which may be stale.

`pringle sync` runs the same reconciliation engine explicitly: it follows each tracked singleton
(NFT, option) forward from its recorded coin to its current live coin on-chain, derives a p2
singleton for every tracked NFT, refreshes each p2 singleton's funded coins (retaining
still-pending ones), updates each record's state, and prunes transactions whose watch coin has
confirmed. It is fault-tolerant: an RPC failure for one asset becomes a warning and never deletes
or downgrades local state, and a missing (pending) watch coin keeps its transaction in the log
rather than pretending it settled.

Run `sync` whenever a command reports a coin is "not confirmed" or "already spent" — the local
record is likely pointing at a coin that has since been replaced by its singleton child.

Each transaction is signed locally with the wallet's synthetic key using the mainnet
`AGG_SIG_ME` constants and pushed with the coinset `push_tx` endpoint. Submitting is not the same
as confirming: after submitting, wait for confirmation (`pringle status`) before the next step.
