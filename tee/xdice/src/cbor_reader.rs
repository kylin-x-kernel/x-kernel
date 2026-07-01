// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::cbor_cert_op::CborType;

pub struct CborIn<'a> {
    pub buffer: &'a [u8],
    pub cursor: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CborReadResult {
    Ok,
    End,
    Malformed,
    NotFound,
}

impl<'a> CborIn<'a> {
    pub fn new(buffer: &'a [u8]) -> Self {
        CborIn { buffer, cursor: 0 }
    }

    pub fn offset(&self) -> usize {
        self.cursor
    }

    pub fn is_at_end(&self) -> bool {
        self.cursor == self.buffer.len()
    }

    pub fn read_would_overflow(&self, size: usize) -> bool {
        match self.cursor.checked_add(size) {
            Some(end_pos) => end_pos > self.buffer.len(),
            None => true,
        }
    }

    pub fn peek_initial_value_and_argument(&self) -> Result<(u8, CborType, u64), CborReadResult> {
        if self.cursor >= self.buffer.len() {
            return Err(CborReadResult::End);
        }

        let initial_byte = self.buffer[self.cursor];

        let cbor_type = match initial_byte >> 5 {
            0 => CborType::UnsignedInt,
            1 => CborType::NegativeInt,
            2 => CborType::ByteString,
            3 => CborType::TextString,
            4 => CborType::Array,
            5 => CborType::Map,
            6 => CborType::Tag,
            7 => CborType::Simple,
            _ => return Err(CborReadResult::Malformed),
        };

        let additional_info = initial_byte & 0x1f;
        let mut bytes: u8 = 1;
        let value: u64;

        if additional_info <= 23 {
            value = additional_info as u64;
        } else if (24..=27).contains(&additional_info) {
            bytes = 1 + (1 << (additional_info - 24));

            if self.read_would_overflow(bytes as usize) {
                return Err(CborReadResult::End);
            }

            let start = self.cursor + 1;
            let end = self.cursor + bytes as usize;
            let data = &self.buffer[start..end];

            value = match additional_info {
                24 => data[0] as u64,
                25 => u16::from_be_bytes([data[0], data[1]]) as u64,
                26 => u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64,
                27 => u64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]),
                _ => unreachable!(),
            };
        } else {
            return Err(CborReadResult::Malformed);
        }

        Ok((bytes, cbor_type, value))
    }

    pub fn read_size(&mut self, expected_type: CborType) -> Result<usize, CborReadResult> {
        let (bytes, actual_type, raw_val) = self.peek_initial_value_and_argument()?;

        if actual_type != expected_type {
            return Err(CborReadResult::NotFound);
        }

        if raw_val > usize::MAX as u64 {
            return Err(CborReadResult::Malformed);
        }

        let size = raw_val as usize;

        self.cursor += bytes as usize;

        Ok(size)
    }

    pub fn read_str(&mut self, expected_type: CborType) -> Result<&'a [u8], CborReadResult> {
        let mut peeker = CborIn {
            buffer: self.buffer,
            cursor: self.cursor,
        };

        let size = peeker.read_size(expected_type)?;

        if peeker.read_would_overflow(size) {
            return Err(CborReadResult::End);
        }

        let start = peeker.cursor;
        let end = start + size;
        let data = &self.buffer[start..end];

        self.cursor = end;

        Ok(data)
    }

    pub fn read_simple(&mut self, expected_val: u8) -> Result<(), CborReadResult> {
        let (bytes, actual_type, raw_val) = self.peek_initial_value_and_argument()?;

        if actual_type != CborType::Simple || raw_val != expected_val as u64 {
            return Err(CborReadResult::NotFound);
        }

        self.cursor += bytes as usize;
        Ok(())
    }

    pub fn read_int(&mut self) -> Result<i64, CborReadResult> {
        let (bytes, actual_type, raw_val) = self.peek_initial_value_and_argument()?;

        if actual_type != CborType::UnsignedInt && actual_type != CborType::NegativeInt {
            return Err(CborReadResult::NotFound);
        }

        if raw_val > i64::MAX as u64 && actual_type == CborType::UnsignedInt {
            return Err(CborReadResult::Malformed);
        }

        let val = if actual_type == CborType::NegativeInt {
            -1i64
                .checked_sub(raw_val as i64)
                .ok_or(CborReadResult::Malformed)?
        } else {
            raw_val as i64
        };

        self.cursor += bytes as usize;
        Ok(val)
    }

    pub fn read_uint(&mut self) -> Result<u64, CborReadResult> {
        let (bytes, actual_type, raw_val) = self.peek_initial_value_and_argument()?;

        if actual_type != CborType::UnsignedInt {
            return Err(CborReadResult::NotFound);
        }

        self.cursor += bytes as usize;
        Ok(raw_val)
    }

    pub fn read_bstr(&mut self) -> Result<&'a [u8], CborReadResult> {
        self.read_str(CborType::ByteString)
    }

    pub fn read_tstr(&mut self) -> Result<&'a [u8], CborReadResult> {
        self.read_str(CborType::TextString)
    }

    pub fn read_array(&mut self) -> Result<usize, CborReadResult> {
        self.read_size(CborType::Array)
    }

    pub fn read_map(&mut self) -> Result<usize, CborReadResult> {
        self.read_size(CborType::Map)
    }

    pub fn read_tag(&mut self) -> Result<u64, CborReadResult> {
        match self.peek_initial_value_and_argument() {
            Ok((bytes, CborType::Tag, tag_val)) => {
                self.cursor += bytes as usize;
                Ok(tag_val)
            }
            Ok(_) => Err(CborReadResult::NotFound),
            Err(e) => Err(e),
        }
    }

    pub fn read_null(&mut self) -> Result<(), CborReadResult> {
        self.read_simple(22)
    }

    pub fn read_skip(&mut self) -> Result<(), CborReadResult> {
        const STACK_SIZE: usize = 16;
        let mut size_stack = [0usize; STACK_SIZE];
        let mut stack_depth: usize = 0;

        let mut peeker_cursor = self.cursor;

        size_stack[stack_depth] = 1;
        stack_depth += 1;

        while stack_depth > 0 {
            let (bytes, cbor_type, val) = self.peek_at(peeker_cursor)?;

            peeker_cursor += bytes as usize;

            size_stack[stack_depth - 1] -= 1;
            if size_stack[stack_depth - 1] == 0 {
                stack_depth -= 1;
            }

            let next_nesting_count: u64 = match cbor_type {
                CborType::UnsignedInt | CborType::NegativeInt | CborType::Simple => 0,
                CborType::ByteString | CborType::TextString => {
                    if val > usize::MAX as u64
                        || self.peek_would_overflow_at(peeker_cursor, val as usize)
                    {
                        return Err(CborReadResult::End);
                    }
                    peeker_cursor += val as usize;
                    0
                }
                CborType::Map => val.checked_mul(2).ok_or(CborReadResult::Malformed)?,
                CborType::Tag => 1,
                CborType::Array => val,
            };

            if next_nesting_count > 0 {
                if stack_depth == STACK_SIZE {
                    return Err(CborReadResult::Malformed);
                }
                if next_nesting_count > usize::MAX as u64 {
                    return Err(CborReadResult::End);
                }
                size_stack[stack_depth] = next_nesting_count as usize;
                stack_depth += 1;
            }
        }

        self.cursor = peeker_cursor;
        Ok(())
    }

    fn peek_at(&self, pos: usize) -> Result<(u8, CborType, u64), CborReadResult> {
        let tmp_in = CborIn {
            buffer: self.buffer,
            cursor: pos,
        };
        tmp_in.peek_initial_value_and_argument()
    }

    fn peek_would_overflow_at(&self, pos: usize, size: usize) -> bool {
        match pos.checked_add(size) {
            Some(end) => end > self.buffer.len(),
            None => true,
        }
    }
}
