use core::fmt::Write;

use heapless::String;

fn field_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let colon = rest.find(':')?;
    Some(rest[colon + 1..].trim_start())
}

fn parse_number(text: &str) -> Option<u32> {
    let text = text.trim();
    let text = if let Some(quoted) = text.strip_prefix('"') {
        &quoted[..quoted.find('"')?]
    } else {
        let end = text
            .find(|ch: char| !(ch.is_ascii_hexdigit() || ch == 'x' || ch == 'X'))
            .unwrap_or(text.len());
        &text[..end]
    };

    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

#[cfg(feature = "can")]
pub(crate) fn parse_bool_field(text: &str, key: &str) -> Option<bool> {
    let value = field_value(text, key)?;

    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn parse_u32_field(text: &str, key: &str) -> Option<u32> {
    parse_number(field_value(text, key)?)
}

#[cfg(feature = "can")]
pub(crate) fn parse_str_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let value = field_value(text, key)?.strip_prefix('"')?;
    Some(&value[..value.find('"')?])
}

pub(crate) fn parse_u8_array<const N: usize>(
    text: &str,
    key: &str,
    out: &mut [u8; N],
) -> Option<usize> {
    let rest = field_value(text, key)?;
    let start = rest.find('[')?;
    let end = rest[start + 1..].find(']')? + start + 1;
    let mut count = 0;

    for part in rest[start + 1..end].split(',') {
        let value = part.trim();
        if value.is_empty() {
            continue;
        }
        if count == out.len() {
            return None;
        }
        out[count] = u8::try_from(parse_number(value)?).ok()?;
        count += 1;
    }

    Some(count)
}

pub(crate) fn write_error<const N: usize>(out: &mut String<N>, code: &str, message: &str) {
    let _ = core::write!(
        out,
        "{{\"type\":\"error\",\"ok\":false,\"code\":\"{}\",\"message\":\"{}\"}}",
        code,
        message
    );
}
