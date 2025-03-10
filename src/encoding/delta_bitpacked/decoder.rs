use crate::encoding::ceil8;

use super::super::bitpacking;
use super::super::uleb128;
use super::super::zigzag_leb128;

#[derive(Debug)]
struct Block<'a> {
    // this is the minimum delta that must be added to every value.
    min_delta: i64,
    num_mini_blocks: usize,
    values_per_mini_block: usize,
    bitwidths: &'a [u8],
    values: &'a [u8],
    remaining: usize,     // number of elements
    current_index: usize, // invariant: < values_per_mini_block
    // None represents a relative delta of zero, in which case there is no miniblock.
    current_miniblock: Option<bitpacking::Decoder<'a>>,
    // number of bytes consumed.
    consumed_bytes: usize,
}

impl<'a> Block<'a> {
    pub fn new(
        mut values: &'a [u8],
        num_mini_blocks: usize,
        values_per_mini_block: usize,
        length: usize,
    ) -> Self {
        let length = std::cmp::min(length, num_mini_blocks * values_per_mini_block);

        let mut consumed_bytes = 0;
        let (min_delta, consumed) = zigzag_leb128::decode(values);
        consumed_bytes += consumed;
        values = &values[consumed..];

        let mut bitwidths = &values[..num_mini_blocks];
        consumed_bytes += num_mini_blocks;
        values = &values[num_mini_blocks..];

        // read first bitwidth
        let num_bits = bitwidths[0];
        bitwidths = &bitwidths[1..];

        let current_miniblock = if num_bits > 0 {
            let length = std::cmp::min(length, values_per_mini_block);
            consumed_bytes += ceil8(values_per_mini_block * num_bits as usize);
            Some(bitpacking::Decoder::new(values, num_bits, length))
        } else {
            None
        };

        Self {
            min_delta,
            num_mini_blocks,
            values_per_mini_block,
            bitwidths,
            remaining: length,
            values,
            current_index: 0,
            current_miniblock,
            consumed_bytes,
        }
    }
}

impl<'a> Iterator for Block<'a> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let result = self.min_delta
            + self
                .current_miniblock
                .as_mut()
                .map(|x| x.next().unwrap())
                .unwrap_or(0) as i64;
        self.current_index += 1;
        self.remaining -= 1;

        if self.remaining > 0 && self.current_index == self.values_per_mini_block {
            let num_bits = self.bitwidths[0];
            self.bitwidths = &self.bitwidths[1..];
            self.current_miniblock = if num_bits > 0 {
                let length = std::cmp::min(self.remaining, self.values_per_mini_block);
                self.consumed_bytes += ceil8(self.values_per_mini_block * num_bits as usize);
                Some(bitpacking::Decoder::new(self.values, num_bits, length))
            } else {
                None
            };
            self.current_index = 0;
        }

        Some(result as u32)
    }
}

/// Decoder of parquets' `DELTA_BINARY_PACKED`. Implements `Iterator<Item = i32>`.
/// # Implementation
/// This struct does not allocate on the heap.
#[derive(Debug)]
pub struct Decoder<'a> {
    block_size: u64,
    num_mini_blocks: usize,
    values_per_mini_block: usize,
    total_count: usize, // total number of elements
    first_value: i64,   // the cumulative
    values: &'a [u8],
    current_block: Block<'a>,
    // the total number of bytes consumed up to a given point, excluding the bytes on the current_block
    consumed_bytes: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(mut values: &'a [u8]) -> Self {
        let mut consumed_bytes = 0;
        let (block_size, consumed) = uleb128::decode(values);
        consumed_bytes += consumed;
        assert_eq!(block_size % 128, 0);
        values = &values[consumed..];
        let (num_mini_blocks, consumed) = uleb128::decode(values);
        let num_mini_blocks = num_mini_blocks as usize;
        consumed_bytes += consumed;
        values = &values[consumed..];
        let (total_count, consumed) = uleb128::decode(values);
        let total_count = total_count as usize;
        consumed_bytes += consumed;
        values = &values[consumed..];
        let (first_value, consumed) = zigzag_leb128::decode(values);
        consumed_bytes += consumed;
        values = &values[consumed..];

        let values_per_mini_block = block_size as usize / num_mini_blocks;
        assert_eq!(values_per_mini_block % 8, 0);

        let current_block = Block::new(
            values,
            num_mini_blocks as usize,
            values_per_mini_block,
            total_count,
        );
        Self {
            block_size,
            num_mini_blocks,
            values_per_mini_block,
            total_count,
            first_value,
            values,
            current_block,
            consumed_bytes,
        }
    }

    /// Returns the total number of bytes consumed up to this point by [`Decoder`].
    pub fn consumed_bytes(&self) -> usize {
        self.consumed_bytes + self.current_block.consumed_bytes
    }
}

impl<'a> Iterator for Decoder<'a> {
    type Item = i32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.total_count == 0 {
            return None;
        }
        let delta = if let Some(x) = self.current_block.next() {
            x as i64
        } else {
            // load next block
            self.values = &self.values[self.current_block.consumed_bytes..];
            self.consumed_bytes += self.current_block.consumed_bytes;
            self.current_block = Block::new(
                self.values,
                self.num_mini_blocks,
                self.values_per_mini_block,
                self.total_count,
            );
            // block is never empty because `self.total_count > 0` at this point, so this is infalible
            self.current_block.next().unwrap() as i64
        };
        self.total_count -= 1;

        let result = Some(self.first_value as i32);
        self.first_value += delta;
        result
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.total_count as usize, Some(self.total_count as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_spec() {
        let expected = (1i32..=5).collect::<Vec<_>>();
        // VALIDATED FROM SPARK==3.1.1
        // header: [128, 1, 4, 5, 2]
        // block size: 128, 1
        // mini-blocks: 4
        // elements: 5
        // first_value: 2 <=z> 1
        // block1: [2, 0, 0, 0, 0]
        // min_delta: 2 <=z> 1
        // bit_width: 0
        let data = &[128, 1, 4, 5, 2, 2, 0, 0, 0, 0];

        let mut decoder = Decoder::new(data);
        let r = decoder.by_ref().collect::<Vec<_>>();

        assert_eq!(expected, r);

        assert_eq!(decoder.consumed_bytes(), 10);
    }

    #[test]
    fn case2() {
        let expected = vec![1i32, 2, 3, 4, 5, 1];
        // VALIDATED FROM SPARK==3.1.1
        // header: [128, 1, 4, 6, 2]
        // block size: 128, 1 <=u> 128
        // mini-blocks: 4     <=u> 4
        // elements: 6        <=u> 6
        // first_value: 2     <=z> 1
        // block1: [7, 3, 0, 0, 0]
        // min_delta: 7       <=z> -4
        // bit_widths: [3, 0, 0, 0]
        // values: [
        //      0b01101101
        //      0b00001011
        //      ...
        // ]                  <=b> [3, 3, 3, 3, 0]
        let data = &[
            128, 1, 4, 6, 2, 7, 3, 0, 0, 0, 0b01101101, 0b00001011, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            // these should not be consumed
            1, 2, 3,
        ];

        let mut decoder = Decoder::new(data);
        let r = decoder.by_ref().collect::<Vec<_>>();

        assert_eq!(expected, r);
        assert_eq!(decoder.consumed_bytes(), data.len() - 3);
    }
}
