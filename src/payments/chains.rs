//! Static facts about well-known EVM chains, used to build explorer links.
//! Unknown chains simply have no links.
use serde_json::{Value, json};

/// Block explorer origins by EIP-155 chain id.
const EXPLORERS: [(u64, &str); 17] = [
    (1, "https://etherscan.io"),
    (10, "https://optimistic.etherscan.io"),
    (56, "https://bscscan.com"),
    (100, "https://gnosisscan.io"),
    (130, "https://uniscan.xyz"),
    (137, "https://polygonscan.com"),
    (250, "https://ftmscan.com"),
    (324, "https://explorer.zksync.io"),
    (8453, "https://basescan.org"),
    (42161, "https://arbiscan.io"),
    (42220, "https://celoscan.io"),
    (43114, "https://snowtrace.io"),
    (57073, "https://explorer.inkonchain.com"),
    (59144, "https://lineascan.build"),
    (534352, "https://scrollscan.com"),
    (81457, "https://blastscan.io"),
    (146, "https://sonicscan.org"),
];

/// The numeric EIP-155 id of a CAIP-2 chain id such as `eip155:8453`.
pub fn eip155(chain: &str) -> Option<u64> {
    chain.strip_prefix("eip155:")?.parse().ok()
}

/// The explorer origin of `chain`, without a trailing slash.
pub fn explorer(chain: &str) -> Option<&'static str> {
    let id = eip155(chain)?;
    EXPLORERS.iter().find(|(c, _)| *c == id).map(|(_, u)| *u)
}

/// The token contract of a CAIP-19 asset id such as `eip155:1/erc20:0xabc`.
/// Native coins (`slip44`) and unknown namespaces have none.
pub fn contract(asset: &str) -> Option<&str> {
    asset.split_once("/erc20:").map(|(_, c)| c)
}

/// Explorer link templates for `chain`, or null. `{address}`, `{token}` and
/// `{tx}` are placeholders.
pub fn links(chain: &str) -> Value {
    match explorer(chain) {
        Some(base) => json!({
            "name": base.trim_start_matches("https://"),
            "address": format!("{base}/address/{{address}}"),
            "token": format!("{base}/token/{{token}}"),
            "tx": format!("{base}/tx/{{tx}}"),
        }),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_chains_have_explorers_and_unknown_ones_do_not() {
        assert_eq!(explorer("eip155:8453"), Some("https://basescan.org"));
        assert_eq!(explorer("eip155:137"), Some("https://polygonscan.com"));
        assert_eq!(explorer("eip155:42161"), Some("https://arbiscan.io"));
        assert_eq!(explorer("eip155:1"), Some("https://etherscan.io"));
        assert_eq!(explorer("eip155:999999"), None);
        assert_eq!(explorer("solana:abc"), None);
        assert!(links("eip155:999999").is_null());
        assert_eq!(
            links("eip155:1")["token"],
            "https://etherscan.io/token/{token}"
        );
    }

    #[test]
    fn contracts_come_from_erc20_asset_ids_only() {
        assert_eq!(contract("eip155:1/erc20:0xabc"), Some("0xabc"));
        assert_eq!(contract("eip155:1/slip44:60"), None);
    }
}
