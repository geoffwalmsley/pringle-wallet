//! Mocked coinset response tests.
//!
//! These feed JSON exactly as the coinset.org RPC returns it into the SDK response
//! types, then exercise the pure interpretation helpers used by the client wrapper.
//! No network access occurs.

use chia_wallet_sdk::coinset::{GetCoinRecordsResponse, PushTxResponse};
use pringle_wallet::coinset::{filter_unspent, interpret_push};

fn hex64() -> String {
    "11".repeat(32)
}

#[test]
fn filters_out_spent_and_unconfirmed_coins() {
    let h = hex64();
    let json = format!(
        r#"{{
            "success": true,
            "error": null,
            "truncated": null,
            "next_cursor": null,
            "coin_records": [
                {{
                    "coin": {{ "parent_coin_info": "0x{h}", "puzzle_hash": "0x{h}", "amount": 100 }},
                    "coinbase": false,
                    "confirmed_block_index": 10,
                    "spent": false,
                    "spent_block_index": 0,
                    "timestamp": 1
                }},
                {{
                    "coin": {{ "parent_coin_info": "0x{h}", "puzzle_hash": "0x{h}", "amount": 200 }},
                    "coinbase": false,
                    "confirmed_block_index": 11,
                    "spent": true,
                    "spent_block_index": 12,
                    "timestamp": 2
                }},
                {{
                    "coin": {{ "parent_coin_info": "0x{h}", "puzzle_hash": "0x{h}", "amount": 300 }},
                    "coinbase": false,
                    "confirmed_block_index": 0,
                    "spent": false,
                    "spent_block_index": 0,
                    "timestamp": 0
                }}
            ]
        }}"#
    );

    let response: GetCoinRecordsResponse = serde_json::from_str(&json).unwrap();
    assert!(response.success);

    let coins = filter_unspent(response.coin_records.unwrap());
    // Only the first coin is confirmed and unspent.
    assert_eq!(coins.len(), 1);
    assert_eq!(coins[0].amount, 100);
}

#[test]
fn accepts_successful_push() {
    let response: PushTxResponse =
        serde_json::from_str(r#"{ "success": true, "status": "SUCCESS", "error": null }"#).unwrap();
    assert!(interpret_push(response).is_ok());
}

#[test]
fn rejects_failed_push_and_surfaces_error() {
    let response: PushTxResponse = serde_json::from_str(
        r#"{ "success": false, "status": "PENDING", "error": "DOUBLE_SPEND" }"#,
    )
    .unwrap();

    let err = interpret_push(response).unwrap_err();
    assert!(err.to_string().contains("DOUBLE_SPEND"));
}
