//! The decoder side of a QPACK dynamic table (RFC 9204), for a server that
//! offers one: it reads the client's encoder stream, decodes field sections
//! that refer to the table, and records how each was encoded.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::h2::hpack_int;
use crate::h3::{prefix_int, static_entry, string_huffman};

/// A dynamic table a server offers its clients: the values of
/// SETTINGS_QPACK_MAX_TABLE_CAPACITY and SETTINGS_QPACK_BLOCKED_STREAMS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QpackOffer {
    pub capacity: u64,
    pub blocked_streams: u64,
}

/// One instruction on the client's encoder stream (RFC 9204 §4.3), as
/// sent: a dynamic index is relative, as on the wire, and each string
/// says whether it was Huffman-coded. Names and values are resolved for
/// reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EncoderInstruction {
    SetCapacity {
        capacity: u64,
    },
    InsertWithNameReference {
        static_table: bool,
        index: u64,
        name: String,
        value: String,
        huffman: bool,
    },
    InsertWithLiteralName {
        name: String,
        value: String,
        huffman_name: bool,
        huffman_value: bool,
    },
    Duplicate {
        index: u64,
        name: String,
        value: String,
    },
}

/// How one request's field section was encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSection {
    /// Required Insert Count, decoded.
    pub required_insert_count: u64,
    /// Base, decoded: above Required Insert Count only if the section
    /// refers to entries after it (post-base).
    pub base: u64,
    /// Whether the section came before the inserts it needed, so the
    /// stream was blocked.
    pub blocked: bool,
    pub lines: Vec<FieldLine>,
}

/// One field line of a section (RFC 9204 §4.5): what it refers to, as
/// sent, and whether its strings were Huffman-coded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldLine {
    /// "static", "dynamic" or "post_base" for an indexed line;
    /// "static_name", "dynamic_name" or "post_base_name" for a literal
    /// value with a name reference; "literal" for a literal name.
    pub kind: String,
    /// The index as sent: relative for a dynamic or post-base one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    pub name: String,
    pub value: String,
    /// The N bit: never to be put in a table on the way on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub never_indexed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huffman_name: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huffman_value: Option<bool>,
}

/// A field section decoded: its fields, how it was encoded, and what the
/// decoder stream owes the client for it.
type Decoded = (Vec<(String, String)>, FieldSection, Vec<u8>);

/// The table as the server's decoder has it.
#[derive(Debug, Default)]
pub struct Table {
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY, which Required Insert Count is
    /// encoded against.
    max_capacity: u64,
    capacity: u64,
    size: u64,
    /// The newest last.
    entries: VecDeque<(String, String)>,
    dropped: u64,
    /// How many entries the client has been told it may count on.
    known_received: u64,
}

fn entry_size(name: &str, value: &str) -> u64 {
    (name.len() + value.len() + 32) as u64
}

impl Table {
    pub fn new(max_capacity: u64) -> Table {
        Table {
            max_capacity,
            ..Table::default()
        }
    }

    pub fn inserted(&self) -> u64 {
        self.dropped + self.entries.len() as u64
    }

    fn get(&self, absolute: u64) -> Option<(String, String)> {
        let at = absolute.checked_sub(self.dropped)?;
        self.entries.get(at as usize).cloned()
    }

    fn insert(&mut self, name: String, value: String) -> Option<()> {
        let size = entry_size(&name, &value);
        if size > self.capacity {
            return None;
        }
        while self.size + size > self.capacity {
            let (n, v) = self.entries.pop_front()?;
            self.size -= entry_size(&n, &v);
            self.dropped += 1;
        }
        self.size += size;
        self.entries.push_back((name, value));
        Some(())
    }

    fn set_capacity(&mut self, capacity: u64) -> Option<()> {
        if capacity > self.max_capacity {
            return None;
        }
        self.capacity = capacity;
        while self.size > capacity {
            let (n, v) = self.entries.pop_front()?;
            self.size -= entry_size(&n, &v);
            self.dropped += 1;
        }
        Some(())
    }

    /// Reads whole instructions from the front of `data`, applies them,
    /// and returns them. What is left is an instruction not all there
    /// yet. `None` for one the table can't take: an encoder stream error.
    pub fn read_instructions(&mut self, data: &mut &[u8]) -> Option<Vec<EncoderInstruction>> {
        let mut out = Vec::new();
        while let Some(&b) = data.first() {
            let mut d = *data;
            let Some(instruction) = self.parse(b, &mut d) else {
                // Not all there, or bad: told apart by whether more data
                // could make it whole, which only the caller knows; a bad
                // one shows as a stream that never moves on.
                break;
            };
            match &instruction {
                EncoderInstruction::SetCapacity { capacity } => self.set_capacity(*capacity)?,
                EncoderInstruction::InsertWithNameReference { name, value, .. }
                | EncoderInstruction::InsertWithLiteralName { name, value, .. }
                | EncoderInstruction::Duplicate { name, value, .. } => {
                    self.insert(name.clone(), value.clone())?
                }
            }
            out.push(instruction);
            *data = d;
        }
        Some(out)
    }

    fn parse(&self, b: u8, d: &mut &[u8]) -> Option<EncoderInstruction> {
        Some(if b & 0x80 != 0 {
            let static_table = b & 0x40 != 0;
            let index = prefix_int(d, 6)?;
            let (value, huffman) = string_huffman(d, 7)?;
            let (name, _) = if static_table {
                static_entry(index)?
            } else {
                self.get(self.inserted().checked_sub(index + 1)?)?
            };
            EncoderInstruction::InsertWithNameReference {
                static_table,
                index,
                name,
                value,
                huffman,
            }
        } else if b & 0x40 != 0 {
            let (name, huffman_name) = string_huffman(d, 5)?;
            let (value, huffman_value) = string_huffman(d, 7)?;
            EncoderInstruction::InsertWithLiteralName {
                name,
                value,
                huffman_name,
                huffman_value,
            }
        } else if b & 0x20 != 0 {
            EncoderInstruction::SetCapacity {
                capacity: prefix_int(d, 5)?,
            }
        } else {
            let index = prefix_int(d, 5)?;
            let (name, value) = self.get(self.inserted().checked_sub(index + 1)?)?;
            EncoderInstruction::Duplicate { index, name, value }
        })
    }

    /// The Required Insert Count a field section's prefix asks for
    /// (RFC 9204 §4.5.1.1), so a caller can wait for those inserts.
    pub fn required_insert_count(&self, block: &[u8]) -> Option<u64> {
        let mut block = block;
        let encoded = prefix_int(&mut block, 8)?;
        if encoded == 0 {
            return Some(0);
        }
        let max_entries = self.max_capacity / 32;
        let full_range = 2 * max_entries;
        if encoded > full_range {
            return None;
        }
        let max_value = self.inserted() + max_entries;
        let max_wrapped = (max_value / full_range) * full_range;
        let mut count = max_wrapped + encoded - 1;
        if count > max_value {
            if count <= full_range {
                return None;
            }
            count -= full_range;
        }
        (count > 0).then_some(count)
    }

    /// Decodes a field section whose inserts are all in, records how it
    /// was encoded, and returns what the decoder stream should carry for
    /// it, as QUICHE's `QpackDecoder::OnDecodingCompleted` sends: a
    /// Section Acknowledgment if it referred to the table, then an Insert
    /// Count Increment for whatever inserts that leaves unacknowledged.
    pub fn decode(&mut self, stream: u64, mut block: &[u8], blocked: bool) -> Option<Decoded> {
        let required_insert_count = self.required_insert_count(block)?;
        if required_insert_count > self.inserted() {
            return None;
        }
        prefix_int(&mut block, 8)?;
        let negative = block.first()? & 0x80 != 0;
        let delta = prefix_int(&mut block, 7)?;
        let base = if negative {
            required_insert_count.checked_sub(delta + 1)?
        } else {
            required_insert_count + delta
        };
        let dynamic = |relative: u64| self.get(base.checked_sub(relative + 1)?);
        let post_base = |relative: u64| self.get(base + relative);
        let mut fields = Vec::new();
        let mut lines = Vec::new();
        while let Some(&b) = block.first() {
            let line = if b & 0x80 != 0 {
                let is_static = b & 0x40 != 0;
                let index = prefix_int(&mut block, 6)?;
                let (name, value) = if is_static {
                    static_entry(index)?
                } else {
                    dynamic(index)?
                };
                FieldLine {
                    kind: if is_static { "static" } else { "dynamic" }.into(),
                    index: Some(index),
                    name,
                    value,
                    never_indexed: false,
                    huffman_name: None,
                    huffman_value: None,
                }
            } else if b & 0x40 != 0 {
                let never_indexed = b & 0x20 != 0;
                let is_static = b & 0x10 != 0;
                let index = prefix_int(&mut block, 4)?;
                let (name, _) = if is_static {
                    static_entry(index)?
                } else {
                    dynamic(index)?
                };
                let (value, huffman) = string_huffman(&mut block, 7)?;
                FieldLine {
                    kind: if is_static {
                        "static_name"
                    } else {
                        "dynamic_name"
                    }
                    .into(),
                    index: Some(index),
                    name,
                    value,
                    never_indexed,
                    huffman_name: None,
                    huffman_value: Some(huffman),
                }
            } else if b & 0x20 != 0 {
                let never_indexed = b & 0x10 != 0;
                let (name, huffman_name) = string_huffman(&mut block, 3)?;
                let (value, huffman_value) = string_huffman(&mut block, 7)?;
                FieldLine {
                    kind: "literal".into(),
                    index: None,
                    name,
                    value,
                    never_indexed,
                    huffman_name: Some(huffman_name),
                    huffman_value: Some(huffman_value),
                }
            } else if b & 0x10 != 0 {
                let index = prefix_int(&mut block, 4)?;
                let (name, value) = post_base(index)?;
                FieldLine {
                    kind: "post_base".into(),
                    index: Some(index),
                    name,
                    value,
                    never_indexed: false,
                    huffman_name: None,
                    huffman_value: None,
                }
            } else {
                let never_indexed = b & 0x08 != 0;
                let index = prefix_int(&mut block, 3)?;
                let (name, _) = post_base(index)?;
                let (value, huffman) = string_huffman(&mut block, 7)?;
                FieldLine {
                    kind: "post_base_name".into(),
                    index: Some(index),
                    name,
                    value,
                    never_indexed,
                    huffman_name: None,
                    huffman_value: Some(huffman),
                }
            };
            fields.push((line.name.clone(), line.value.clone()));
            lines.push(line);
        }
        let mut decoder_stream = Vec::new();
        if required_insert_count > 0 {
            // Section Acknowledgment.
            hpack_int(&mut decoder_stream, 0x80, 7, stream as usize);
            self.known_received = self.known_received.max(required_insert_count);
        }
        if self.known_received < self.inserted() {
            // Insert Count Increment.
            hpack_int(
                &mut decoder_stream,
                0x00,
                6,
                (self.inserted() - self.known_received) as usize,
            );
            self.known_received = self.inserted();
        }
        let section = FieldSection {
            required_insert_count,
            base,
            blocked,
            lines,
        };
        Some((fields, section, decoder_stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9204 Appendix B.2's encoder stream, then its field section.
    #[test]
    fn rfc_9204_b2_decodes() {
        let mut table = Table::new(220);
        let encoder = [
            0x3f, 0xbd, 0x01, // Set Dynamic Table Capacity 220
            0xc0, 0x0f, b'w', b'w', b'w', b'.', b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.',
            b'c', b'o', b'm', // Insert With Name Reference, static 0 (:authority)
            0xc1, 0x0c, b'/', b's', b'a', b'm', b'p', b'l', b'e', b'/', b'p', b'a', b't', b'h',
        ]; // Insert With Name Reference, static 1 (:path)
        let mut data = &encoder[..];
        let got = table.read_instructions(&mut data).unwrap();
        assert!(data.is_empty());
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], EncoderInstruction::SetCapacity { capacity: 220 });
        assert_eq!(table.inserted(), 2);
        // Required Insert Count 2 (encoded 3); sign 1, delta 1, so Base is
        // 2 - 1 - 1 = 0, and both lines are post-base.
        let block = [0x03, 0x81, 0x10, 0x11];
        assert_eq!(table.required_insert_count(&block), Some(2));
        let (fields, section, decoder) = table.decode(4, &block, false).unwrap();
        assert_eq!(
            fields,
            [
                (":authority".to_string(), "www.example.com".to_string()),
                (":path".to_string(), "/sample/path".to_string()),
            ]
        );
        assert_eq!(section.base, 0);
        assert_eq!(section.lines[0].kind, "post_base");
        assert_eq!(decoder, [0x84], "Section Acknowledgment for stream 4");
    }

    #[test]
    fn a_section_after_inserts_it_doesnt_use_acknowledges_them() {
        let mut table = Table::new(4096);
        let mut data = &[0x3f, 0xe1, 0x1f, 0x40 | 1, b'a', 1, b'b'][..];
        table.read_instructions(&mut data).unwrap();
        let (_, section, decoder) = table.decode(0, &[0x00, 0x00, 0xd1], false).unwrap();
        assert_eq!(section.required_insert_count, 0);
        assert_eq!(decoder, [0x01], "Insert Count Increment 1");
    }

    #[test]
    fn an_instruction_cut_short_waits() {
        let mut table = Table::new(4096);
        let mut data = &[0x3f, 0xe1, 0x1f, 0x40 | 3, b'a'][..];
        let got = table.read_instructions(&mut data).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(data, [0x40 | 3, b'a']);
    }

    #[test]
    fn a_dynamic_reference_reads_the_entry() {
        let mut table = Table::new(4096);
        // Capacity 4096, insert "x-a: 1" with a literal name, then a
        // section that refers to it: RIC 1 (encoded 2), Base 1, dynamic 0.
        let mut data = &[0x3f, 0xe1, 0x1f, 0x43, b'x', b'-', b'a', 1, b'1'][..];
        table.read_instructions(&mut data).unwrap();
        let (fields, section, _) = table.decode(8, &[0x02, 0x00, 0x80], false).unwrap();
        assert_eq!(fields, [("x-a".to_string(), "1".to_string())]);
        assert_eq!(section.lines[0].kind, "dynamic");
        assert_eq!(table.required_insert_count(&[0x03, 0x00]), Some(2));
        assert!(
            table.decode(8, &[0x03, 0x00, 0x80], false).is_none(),
            "not in yet"
        );
    }
}
