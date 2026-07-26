//! Potato spend interpretation, driven by a real mainnet spend.
//!
//! The fixture is the coinset `get_puzzle_and_solution` response for potato coin
//! `0xd54d9f…f941` (the 962 XCH pot), which was passed on to the holder of
//! `0x1e54b8…0f0e`. Running its puzzle is pure computation, so no network access occurs.

use chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse;
use chia_wallet_sdk::prelude::{Bytes32, CoinSpend};
use pringle_wallet::potato::{parse_spend, PotatoSpend, PRICE, TIME_LOCK};

const SPENT_POT: u64 = 962_000_000_000_000;
const SUCCESSOR_COIN_ID: &str = "1e54b89f681cad9858161cccce3691e9da085ade64275be21e73f81f7c810f0e";
const HOLDER: &str = "58f47d58b6782b806a16d9a5e0e51288444fba27b37013d0c31bf46fbbf799c0";
const ACQUIRED_AT: u64 = 1_785_044_895;

fn fixture() -> CoinSpend {
    let json = include_str!("fixtures/potato_pass.json");
    let response: GetPuzzleAndSolutionResponse = serde_json::from_str(json).unwrap();
    assert!(response.success);
    response.coin_solution.unwrap()
}

fn hex32(value: &str) -> Bytes32 {
    Bytes32::new(hex::decode(value).unwrap().try_into().unwrap())
}

#[test]
fn reads_the_new_holder_and_purchase_time_from_a_pass() {
    let spend = fixture();
    assert_eq!(spend.coin.amount, SPENT_POT);

    let PotatoSpend::Passed(pass) = parse_spend(&spend).unwrap() else {
        panic!("expected the spend to be a pass");
    };

    assert_eq!(pass.holder, hex32(HOLDER));
    assert_eq!(pass.acquired_at, ACQUIRED_AT);
}

#[test]
fn identifies_the_successor_potato_coin() {
    let PotatoSpend::Passed(pass) = parse_spend(&fixture()).unwrap() else {
        panic!("expected the spend to be a pass");
    };

    // The successor is the coin this whole command follows, so its id must match exactly.
    assert_eq!(pass.coin.coin_id(), hex32(SUCCESSOR_COIN_ID));
    assert_eq!(pass.coin.amount, SPENT_POT + PRICE);
}

#[test]
fn splits_out_the_royalty_coin() {
    let PotatoSpend::Passed(pass) = parse_spend(&fixture()).unwrap() else {
        panic!("expected the spend to be a pass");
    };

    let royalty = pass.royalty.expect("a pass pays earlier holders a royalty");
    assert_eq!(royalty.amount, 962_000_000_000);
    // The royalty is never mistaken for the potato itself.
    assert_ne!(royalty.coin_id(), pass.coin.coin_id());
}

#[test]
fn deadline_is_a_day_after_the_purchase() {
    let PotatoSpend::Passed(pass) = parse_spend(&fixture()).unwrap() else {
        panic!("expected the spend to be a pass");
    };

    assert_eq!(pass.acquired_at + TIME_LOCK, 1_785_131_295);
}
