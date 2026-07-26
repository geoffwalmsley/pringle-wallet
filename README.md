# pringle-wallet

A small, single-binary Rust CLI for creating and trading options on income streams paid to
NFT-controlled `p2 singleton` addresses. It was built primarily to experiment with options on
[potpotato.xyz](https://potpotato.xyz/) income streams.

Pringle uses [`chia-wallet-sdk`](https://crates.io/crates/chia-wallet-sdk) to provide an
end-to-end flow on **Chia mainnet**:

1. Generate and locally store a BLS key, and derive a standard `xch` wallet address.
2. Discover, consolidate, and send regular XCH coins through the public
   [coinset.org](https://api.coinset.org) API.
3. Mint a singleton NFT owned by the wallet.
4. Derive a `p2 singleton` address that receives income and can only be spent with the NFT.
5. Create an option contract whose *underlying* asset is that NFT, with an XCH strike price.
6. Sell (offer/take) or exercise the option.

## Income-stream options

The NFT acts as the control key for its deterministic `p2 singleton` address. Income can continue
arriving at that address while the NFT is locked in an option. Those coins cannot be swept without
co-spending the NFT, so the option writer cannot withdraw them while the NFT remains locked.

The option holder can exercise before expiration by paying the XCH strike. There are two option
**kinds**, chosen at creation, that differ only in what exercise does (see
[Option kinds](#option-kinds)). The default `transfer` kind hands the NFT—and therefore control of
the accumulated p2-singleton balance and any future income sent to the same address—to the
exerciser. The `sweep` kind instead pays the holder only the income accumulated at the moment of
exercise and returns the NFT to the creator. Either way, the traded option coin is an option on the
income stream controlled by the NFT, rather than an option on each individual incoming XCH coin.

This model was designed mainly for potpotato.xyz revenue streams: route the stream to an
NFT-controlled p2 singleton, lock that NFT as the option underlying, and trade the option using a
Chia offer file. Income arrives by sending XCH to the NFT's income address (`pringle nft address`)
from any wallet.

Creating, offering, taking, exercising, sweeping, and clawback are all implemented.
If an option expires unexercised, its creator can reclaim the locked NFT with `pringle option
clawback` (see [Expired options](#expired-options)); no option "melt" is required.

## Option kinds

Every option is one of two kinds, fixed at creation with `pringle option create --kind`:

- **`transfer`** (default) — exercise pays the strike and hands the NFT to the holder, along with
  control of the p2-singleton address: the balance sitting there now *and* every future coin sent
  to it. It is a call on the income stream itself.
- **`sweep`** — exercise pays the same strike but sweeps only the income accumulated at that moment
  to the holder, and returns the NFT to the *creator*. The NFT never changes hands. It is a call on
  the balance accrued by exercise time, not on the ongoing stream.

Both kinds lock the NFT into the exact same underlying puzzle, so offers, taking, `inspect`,
`status`, `sync`, and creator clawback of an expired option all work identically for either. The
only on-chain difference is a single 32-byte value the option commits to, which `pringle option
inspect` / `recover` re-derive and prove from the chain — so a purchased option's kind is a
verified fact, not a claim by the seller. Every place an option is shown (`status`,
`option show-all`, `option inspect`, and the `take`/`exercise` previews) labels the kind and spells
out what exercise will do.

**Single use.** Exercising consumes the option coin. A sweep option therefore captures only the
income present when it is exercised; anything that arrives afterward accrues to the creator, who by
then holds the NFT again. There is no way to exercise a sweep option a second time.

**Per-transaction coin cap.** A p2-singleton coin can only be spent by co-spending the NFT, and the
mempool rejects any single transaction over `MAX_BLOCK_COST_CLVM / 2` (5,500,000,000 cost). Each
co-spent coin adds a fixed chunk of cost, so a sweep can only fold in a bounded number of coins at
once: **662** for a plain `pringle nft sweep` and **655** for a sweep-on-exercise (which also
carries the option-melt, NFT-exercise, and strike-settlement spends). When more coins than the cap
are present, the highest-value coins are swept first and the rest are reported as skipped. For
`pringle nft sweep` the remainder can be swept in a follow-up transaction; for a sweep-*exercise*
the remainder is left at the address and, because the NFT returns to the creator, becomes the
creator's to sweep. These caps are pinned against real mempool cost by `tests/cost.rs`.

**Cosmetic NFT-tampering caveat.** The sweep delegated puzzle forces the NFT back to the creator via
a single odd `CREATE_COIN`; the singleton layer's one-odd-output rule is what stops a holder from
redirecting or melting it. The remainder of the exercise conditions are holder-supplied, so a sweep
holder can additionally emit NFT metadata (`-24`) or owner/royalty (`-10`) conditions on the
returned NFT. This is purely cosmetic: it cannot move the NFT to anyone but the creator, and it
cannot affect the income address, which derives from the launcher id alone.

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
pringle xch address

# 2. Inspect on-chain coins for the wallet.
pringle xch coins
pringle xch coin <coin-id>

# Combine all spendable regular XCH coins into one wallet coin.
pringle xch consolidate --fee 100000

# Send XCH to an address: a specific number of mojos (the fee is paid on top, out of the
# change), or --all to empty the regular wallet by sending the balance minus the fee.
# Funds at p2 singleton addresses are separate and are not included.
pringle xch send xch1... --amount 250000000 --fee 100000
pringle xch send xch1... --all --fee 100000

# 3. Mint an NFT owned by the wallet (fee in mojos).
pringle nft mint --fee 100000

# 4. Derive the NFT's income address (the NFT must be confirmed first). Send XCH to this
# address from any wallet to accumulate income under the NFT's control.
pringle nft address

# Later: sweep the NFT's entire accumulated income (all coins) out in one transaction.
# Defaults to the wallet's own address; pass --address to send elsewhere. The NFT is
# co-spent to authorize the sweep, so it must be wallet-controlled (not locked in an option).
# Because the singleton layer requires exactly one odd output (the recreated NFT), an odd
# remaining mojo is unavoidably donated to the fee; the sweep reports it separately.
pringle nft sweep --fee 100000
pringle nft sweep --address xch1... --fee 100000

# 5. Create an NFT-backed XCH option. --kind selects what exercise does (see "Option kinds"):
#    transfer (default) hands over the NFT; sweep pays out the accrued income and keeps the NFT.
pringle option create --strike 5000000000000 --expiration 1893456000 --fee 100000
pringle option create --kind sweep --strike 5000000000000 --expiration 1893456000 --fee 100000

# 6a. Sell the option: write an offer, or accept one selling an option for XCH.
pringle option offer --request 250000000 --output option.offer
pringle option take option.offer --fee 100000

# Received an offer? Inspect it before buying. This looks the option up on-chain and
# reports its verified terms, its underlying NFT, and the income that NFT's p2 singleton
# is currently holding, then asks whether to take it (see below).
pringle option inspect received.offer --fee 100000

# A purchased option's terms (strike/expiration/creator) aren't in the offer file; `take`
# recovers them from the chain automatically. If the option wasn't confirmed yet, run:
pringle option recover

# 6b. Or, as the option owner, exercise before expiration: pay the strike and either receive the
# NFT (transfer kind) or sweep out the accrued income while the NFT returns to the creator (sweep).
# `show-all` refreshes from the chain, prints full launcher ids, and provides copyable
# exercise commands. By default it hides expired/exercised/transferred options.
pringle option show-all
pringle option show-all --include-closed
pringle option show-all --cached       # local snapshot only
pringle option exercise --fee 100000
# When several options are open, copy the full launcher id from `show-all`:
pringle option exercise --launcher 0x... --fee 100000

# 6c. Or, if the option expires unexercised, the creator reclaims the locked NFT (clawback).
# Only works after the expiration deadline; the expired option coin is left untouched.
pringle option clawback --fee 100000
pringle option clawback --launcher 0x... --address xch1... --fee 100000

# At any point, review local lifecycle state (refreshed against the chain by default).
pringle status
pringle status --cached   # local snapshot only, no network

# Reconcile local state with the chain (fixes stale coin ids after confirmations).
pringle sync
```

### Inspecting an offer before taking it

An offer file names the option it sells but carries none of its terms, so `pringle option
inspect` reads them back off the chain. It reports whether the offer can still settle (the
maker's option coin must be unspent), the strike, expiration, and creator, the underlying
NFT, and the balance held at that NFT's income address — the money that comes with the NFT
when the option is exercised. The terms are only shown once they reproduce the option's
on-chain `underlying_delegated_puzzle_hash`, so a reported strike or expiration is a fact
about the contract rather than a claim by the maker.

Nothing is spent by inspecting. When the offer is live, its terms verify, and the wallet
holds enough XCH to cover the asking price plus `--fee`, the command finishes by asking
whether to take it; answering `y` runs the same purchase as `pringle option take`. Unlike
the confirmation prompts on destructive commands, this one defaults to *no*: a
non-interactive run (or `--json`) prints the report and stops, so a script never buys an
option by accident. `pringle option take` shows the same terms and income figures in its
confirmation preview.

### Expired options

Expiration only disables *exercise* (enforced on-chain by `ASSERT_BEFORE_SECONDS_ABSOLUTE`); it
does **not** move the underlying NFT automatically. If an option you created expires unexercised,
the NFT stays locked in the option underlying until you reclaim it. Because clawback spends only
the underlying's time-locked creator path, the option singleton is never melted — it simply
remains as an inert expired coin.

Reclaiming is a two-step, creator-only flow:

1. `pringle option clawback` — spends the locked NFT through its clawback path (which enforces the
   expiry deadline with `ASSERT_SECONDS_ABSOLUTE` and requires the creator's key) and recreates
   the NFT under your control. Any fee is funded from separate regular-XCH coins.
2. After `pringle status` shows the NFT as *Ready*, run `pringle nft sweep` to withdraw
   the income the NFT's p2 singleton accumulated while the option was live.

`pringle status` and `pringle option show-all --include-closed` flag eligible options with a
copyable clawback command and report a reclaimed option as *Expired (NFT reclaimed)*.

Option offer files use the wallet SDK's option primitive. At the time of writing, Sage does not
support NFT-backed option underlyings, so these offers must be viewed, taken, and exercised with a
compatible tool such as Pringle.

## How status and sync work

`pringle status` reconciles against the chain by default and then reports each asset with an
intuitive state — *Ready*, *Pending confirmation*, *Locked in option*, *Expired*, *Expired (NFT
reclaimed)*, *Exercised*, *Closed*, *Transferred*, *Empty*, or *Unknown* — hiding raw phases and
coin ids unless
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
