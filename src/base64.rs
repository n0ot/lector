pub(crate) fn encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from((second & 0x0f) << 2 | third >> 6)] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0x3f)] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub(crate) fn decode_unpadded(encoded: &[u8]) -> Option<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let digits = encoded
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(digit)
        .collect::<Option<Vec<_>>>()?;
    if digits.len() % 4 == 1 {
        return None;
    }

    let mut decoded = Vec::with_capacity(digits.len().saturating_mul(3) / 4);
    for chunk in digits.chunks(4) {
        decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
        if let Some(third) = chunk.get(2) {
            decoded.push((chunk[1] << 4) | (third >> 2));
            if let Some(fourth) = chunk.get(3) {
                decoded.push((third << 6) | fourth);
            }
        }
    }
    Some(decoded)
}
