//! Just enough protobuf to read a message's fields without its schema compiled in: varints and
//! length-delimited fields (strings, bytes, nested messages), by number.

/// One field: its number and value.
#[derive(Debug, Clone, PartialEq)]
pub enum Field<'a> {
    Varint(u32, u64),
    Bytes(u32, &'a [u8]),
    Fixed(u32, u64),
}

fn varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Every field, in order (stops at anything malformed).
pub fn fields(b: &[u8]) -> Vec<Field<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let Some(key) = varint(b, &mut at) else { break };
        let (num, wire) = ((key >> 3) as u32, key & 7);
        match wire {
            0 => match varint(b, &mut at) {
                Some(v) => out.push(Field::Varint(num, v)),
                None => break,
            },
            1 => {
                let Some(s) = b.get(at..at + 8) else { break };
                out.push(Field::Fixed(num, u64::from_le_bytes(s.try_into().unwrap())));
                at += 8;
            }
            2 => {
                let Some(len) = varint(b, &mut at) else { break };
                let Some(s) = b.get(at..at + len as usize) else { break };
                out.push(Field::Bytes(num, s));
                at += len as usize;
            }
            5 => {
                let Some(s) = b.get(at..at + 4) else { break };
                out.push(Field::Fixed(num, u64::from(u32::from_le_bytes(s.try_into().unwrap()))));
                at += 4;
            }
            _ => break,
        }
    }
    out
}

/// A message read once, looked up by field number.
pub struct Msg<'a>(Vec<Field<'a>>);

impl<'a> Msg<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Msg(fields(b))
    }

    pub fn bytes(&self, n: u32) -> Option<&'a [u8]> {
        self.0.iter().find_map(|f| match f {
            Field::Bytes(k, b) if *k == n => Some(*b),
            _ => None,
        })
    }

    pub fn all_bytes(&self, n: u32) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.0.iter().filter_map(move |f| match f {
            Field::Bytes(k, b) if *k == n => Some(*b),
            _ => None,
        })
    }

    pub fn string(&self, n: u32) -> String {
        self.bytes(n).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
    }

    pub fn int(&self, n: u32) -> u64 {
        self.0
            .iter()
            .find_map(|f| match f {
                Field::Varint(k, v) | Field::Fixed(k, v) if *k == n => Some(*v),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub fn msg(&self, n: u32) -> Msg<'a> {
        Msg::new(self.bytes(n).unwrap_or(&[]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_varints_strings_and_nested() {
        // field 1 = 150 (varint), field 2 = "hi", field 3 = {1: 7}
        let b = [0x08, 0x96, 0x01, 0x12, 0x02, b'h', b'i', 0x1a, 0x02, 0x08, 0x07];
        let m = Msg::new(&b);
        assert_eq!((m.int(1), m.string(2).as_str(), m.msg(3).int(1)), (150, "hi", 7));
    }
}
