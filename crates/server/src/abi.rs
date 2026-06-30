use solsp_syntax::SyntaxKind;

use crate::syntax_utils::type_text;

pub(crate) fn error_selector_hex(error: &solsp_syntax::SyntaxNode) -> Option<String> {
    let signature = abi_signature(error)?;
    let hash = keccak256(signature.as_bytes());
    Some(to_hex(&hash[..4]))
}

pub(crate) fn event_topic_hex(event: &solsp_syntax::SyntaxNode) -> Option<String> {
    let signature = abi_signature(event)?;
    Some(to_hex(&keccak256(signature.as_bytes())))
}

fn abi_signature(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    let name = super::declaration_name(decl)?;
    let params = decl
        .children()
        .find(|node| node.kind() == SyntaxKind::PARAM_LIST)
        .into_iter()
        .flat_map(|list| list.children())
        .filter(|node| node.kind() == SyntaxKind::PARAM)
        .map(|param| canonical_type(&type_text(&param)?))
        .collect::<Option<Vec<_>>>()?;
    Some(format!("{name}({})", params.join(",")))
}

fn canonical_type(ty: &str) -> Option<String> {
    let compact: String = ty.split_whitespace().collect();
    let canonical = canonical_type_inner(&compact);
    (!canonical.is_empty()).then_some(canonical)
}

fn canonical_type_inner(ty: &str) -> String {
    if let Some((base, suffix)) = split_array_suffix(ty) {
        return format!("{}{}", canonical_type_inner(base), suffix);
    }
    match ty {
        "uint" => "uint256".to_string(),
        "int" => "int256".to_string(),
        "fixed" => "fixed128x18".to_string(),
        "ufixed" => "ufixed128x18".to_string(),
        "addresspayable" => "address".to_string(),
        _ => ty.to_string(),
    }
}

fn split_array_suffix(ty: &str) -> Option<(&str, &str)> {
    if !ty.ends_with(']') {
        return None;
    }
    let start = ty.rfind('[')?;
    Some((&ty[..start], &ty[start..]))
}

pub(crate) fn yul_contains_hex(root: &solsp_syntax::SyntaxNode, hex: &str) -> bool {
    let needle = hex.to_ascii_lowercase();
    root.descendants()
        .filter(|node| node.kind() == SyntaxKind::YUL_BLOCK)
        .any(|block| {
            let text: String = block
                .descendants_with_tokens()
                .filter_map(|element| element.into_token())
                .filter(|token| token.kind() != SyntaxKind::COMMENT)
                .map(|token| token.text().to_ascii_lowercase())
                .collect();
            text.contains(&needle)
        })
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn keccak256(input: &[u8]) -> [u8; 32] {
    const RATE: usize = 136;
    let mut state = [0u64; 25];
    let mut chunks = input.chunks_exact(RATE);
    for chunk in &mut chunks {
        absorb_block(&mut state, chunk);
        keccakf(&mut state);
    }

    let rem = chunks.remainder();
    let mut block = [0u8; RATE];
    block[..rem.len()].copy_from_slice(rem);
    block[rem.len()] = 0x01;
    block[RATE - 1] |= 0x80;
    absorb_block(&mut state, &block);
    keccakf(&mut state);

    let mut out = [0u8; 32];
    for (i, lane) in state.iter().take(4).enumerate() {
        out[i * 8..(i + 1) * 8].copy_from_slice(&lane.to_le_bytes());
    }
    out
}

fn absorb_block(state: &mut [u64; 25], block: &[u8]) {
    for (lane, bytes) in block.chunks_exact(8).enumerate() {
        state[lane] ^= u64::from_le_bytes(bytes.try_into().expect("8-byte lane"));
    }
}

fn keccakf(state: &mut [u64; 25]) {
    const ROUNDS: [u64; 24] = [
        0x0000000000000001,
        0x0000000000008082,
        0x800000000000808a,
        0x8000000080008000,
        0x000000000000808b,
        0x0000000080000001,
        0x8000000080008081,
        0x8000000000008009,
        0x000000000000008a,
        0x0000000000000088,
        0x0000000080008009,
        0x000000008000000a,
        0x000000008000808b,
        0x800000000000008b,
        0x8000000000008089,
        0x8000000000008003,
        0x8000000000008002,
        0x8000000000000080,
        0x000000000000800a,
        0x800000008000000a,
        0x8000000080008081,
        0x8000000000008080,
        0x0000000080000001,
        0x8000000080008008,
    ];
    const ROT: [[u32; 5]; 5] = [
        [0, 36, 3, 41, 18],
        [1, 44, 10, 45, 2],
        [62, 6, 43, 15, 61],
        [28, 55, 25, 21, 56],
        [27, 20, 39, 8, 14],
    ];

    for rc in ROUNDS {
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
        }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in (0..25).step_by(5) {
                state[x + y] ^= d;
            }
        }

        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                b[y + 5 * ((2 * x + 3 * y) % 5)] = state[x + 5 * y].rotate_left(ROT[x][y]);
            }
        }

        for y in (0..25).step_by(5) {
            for x in 0..5 {
                state[y + x] = b[y + x] ^ ((!b[y + ((x + 1) % 5)]) & b[y + ((x + 2) % 5)]);
            }
        }

        state[0] ^= rc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_known_error_selector() {
        let hash = keccak256(b"UnsafeDotPosition(uint256)");
        assert_eq!(to_hex(&hash[..4]), "bfb6d3c2");
    }

    #[test]
    fn computes_known_event_topic() {
        assert_eq!(
            to_hex(&keccak256(b"Transfer(address,address,uint256)")),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn canonicalizes_abi_type_aliases() {
        assert_eq!(canonical_type("uint"), Some("uint256".to_string()));
        assert_eq!(canonical_type("int[]"), Some("int256[]".to_string()));
        assert_eq!(
            canonical_type("address payable"),
            Some("address".to_string())
        );
    }
}
