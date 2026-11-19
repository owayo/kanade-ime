// SPDX-License-Identifier: MIT
//! Quantized character probabilities stored in Tsumugi's GGUF metadata.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use kanade_llm::{Job, frame::Fault};
use std::{collections::HashMap, io::Read};

pub(super) const MAX_BYTES: usize = 32 * 1024 * 1024;

struct Table {
    keys: Vec<u64>,
    costs: Vec<u8>,
}

fn take<'a>(data: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (value, rest) = data.split_at_checked(len)?;
    *data = rest;
    Some(value)
}

fn count(data: &mut &[u8]) -> Option<usize> {
    Some(u32::from_le_bytes(take(data, 4)?.try_into().ok()?) as usize)
}

impl Table {
    fn read(data: &mut &[u8]) -> Option<Self> {
        let n = count(data)?;
        let raw = take(data, n.checked_mul(8)?)?;
        let costs = take(data, n)?.to_vec();
        let keys: Vec<_> = raw
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_le_bytes(*bytes))
            .collect();
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return None;
        }
        Some(Self { keys, costs })
    }

    fn get(&self, key: u64) -> Option<i32> {
        self.keys
            .binary_search(&key)
            .ok()
            .map(|i| i32::from(self.costs[i]))
    }
}

pub(super) struct Prior {
    ids: HashMap<char, u64>,
    unigrams: Vec<u8>,
    // Each level contains the backoff cost for a history and the cost of an observed continuation.
    levels: Vec<(Table, Table)>,
    radix: u64,
    history_modulus: u64,
}

impl Prior {
    pub(super) fn decode(encoded: &[u8]) -> Result<Self, Fault> {
        if encoded.len() > MAX_BYTES {
            return Err(Fault::ModelRejected);
        }
        let compressed = STANDARD.decode(encoded).map_err(|_| Fault::ModelRejected)?;
        let mut bytes = Vec::new();
        ZlibDecoder::new(compressed.as_slice())
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Fault::ModelRejected)?;
        if bytes.len() > MAX_BYTES {
            return Err(Fault::ModelRejected);
        }
        Self::parse(&bytes).ok_or(Fault::ModelRejected)
    }

    fn parse(mut data: &[u8]) -> Option<Self> {
        if take(&mut data, 4)? != b"KNG1" {
            return None;
        }
        let radix = count(&mut data)?;
        let order = count(&mut data)?;
        if !(4..=7131).contains(&radix) || order != 5 {
            return None;
        }
        let mut ids = HashMap::new();
        for id in 3..radix {
            let code = u32::from_le_bytes(take(&mut data, 4)?.try_into().ok()?);
            if ids.insert(char::from_u32(code)?, id as u64).is_some() {
                return None;
            }
        }
        let unigrams = take(&mut data, radix)?.to_vec();
        let mut levels = Vec::new();
        for _ in 1..order {
            levels.push((Table::read(&mut data)?, Table::read(&mut data)?));
        }
        data.is_empty().then_some(Self {
            ids,
            unigrams,
            levels,
            radix: radix as u64,
            history_modulus: (radix as u64).pow(order as u32 - 2),
        })
    }

    fn id(&self, c: char) -> u64 {
        self.ids.get(&c).copied().unwrap_or(2)
    }

    fn advance(&self, history: u64, symbol: u64) -> u64 {
        (history % self.history_modulus) * self.radix + symbol
    }

    fn cost(&self, mut history: u64, candidate: &str) -> i32 {
        let mut total = 0;
        for c in candidate.chars() {
            let symbol = self.id(c);
            let mut loss = i32::from(self.unigrams[symbol as usize]);
            let mut modulus = self.radix;
            for (backoff, gram) in &self.levels {
                let context = history % modulus;
                loss = gram
                    .get(context * self.radix + symbol)
                    .unwrap_or_else(|| loss + backoff.get(context).unwrap_or(0));
                modulus *= self.radix;
            }
            total += loss;
            history = self.advance(history, symbol);
        }
        total
    }

    pub(super) fn combine(&self, job: &Job, scores: &mut [Option<f32>]) {
        if job.context.is_empty()
            || !job.prefix.is_empty()
            || !job.suffix.is_empty()
            || scores.len() < 2
            || scores.iter().any(Option::is_none)
        {
            return;
        }
        let history = job
            .context
            .chars()
            .fold(0, |h, c| self.advance(h, self.id(c)));
        let costs: Vec<_> = job
            .candidates
            .iter()
            .map(|text| self.cost(history, text))
            .collect();
        let mut ordered = costs.clone();
        ordered.sort_unstable();
        // Costs are in eighths of a natural-log unit. Skip uncertain or tied comparisons.
        if ordered[1] - ordered[0] < 24 {
            return;
        }
        for (score, cost) in scores.iter_mut().zip(costs) {
            if let Some(value) = score {
                *value -= (cost - ordered[0]) as f32 / 8.0;
            }
        }
    }
}
