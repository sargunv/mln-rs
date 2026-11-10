//! Small buffer writes, gathered into one upload and copied into place
//! ahead of the next frame's commands. A frame writes many small runs
//! (uniforms, symbol vertices), and on Metal wgpu's
//! `Queue::write_buffer` makes a staging buffer for every call, a trip into
//! the kernel each.
//!
//! Writes that touch or overlap in one buffer are merged into one copy, the
//! later write's bytes winning where they overlap, as they would with
//! `Queue::write_buffer`. Each copy is a command for wgpu to track and the
//! driver to encode.
//!
//! Natively the upload goes through a staging belt: mapped buffers that
//! return once the GPU has copied from them. In the browser, where mapping
//! waits for the event loop, it is one `Queue::write_buffer` into a staging
//! buffer that is kept while it's big enough.

use std::ops::Range;

/// Writes up to this size are gathered; larger ones go to the queue.
const GATHERED: usize = 64 << 10;

/// A gathered write: `len` bytes at `offset` in `buffer`, from `at` in the
/// gathered data. Generic over the buffer only for the tests.
#[derive(Clone, Debug)]
struct Write<B> {
    buffer: B,
    offset: u64,
    at: u64,
    len: u64,
}

/// One copy into place: `len` bytes at `offset` in the buffer of its writes,
/// from `at` in the upload, made of the writes `order[writes]`.
#[derive(Clone, Debug)]
struct Run {
    offset: u64,
    at: u64,
    len: u64,
    writes: Range<usize>,
    /// Whether its writes overlap, so must land in the order they came.
    overlapping: bool,
}

#[derive(Debug)]
struct Pending<B> {
    data: Vec<u8>,
    writes: Vec<Write<B>>,
    /// The writes' indices by buffer and offset, and the runs they merge
    /// into: kept for their allocations.
    order: Vec<usize>,
    runs: Vec<Run>,
}

impl<B> Default for Pending<B> {
    fn default() -> Self {
        Self { data: Vec::new(), writes: Vec::new(), order: Vec::new(), runs: Vec::new() }
    }
}

impl<B: Ord> Pending<B> {
    fn gather(&mut self, buffer: B, offset: u64, data: &[u8]) {
        let at = self.data.len() as u64;
        self.data.extend_from_slice(data);
        self.writes.push(Write { buffer, offset, at, len: data.len() as u64 });
    }

    /// Whether a gathered write touches `len` bytes at `offset` of `buffer`:
    /// a write that goes to the queue directly would land before it.
    fn overlaps(&self, buffer: &B, offset: u64, len: u64) -> bool {
        self.writes.iter().any(|w| w.buffer == *buffer && w.offset < offset + len && offset < w.offset + w.len)
    }

    /// The buffer that `run` copies into.
    fn target(&self, run: &Run) -> &B {
        &self.writes[self.order[run.writes.start]].buffer
    }

    /// Merges the writes into runs and returns the upload's size.
    fn plan(&mut self) -> u64 {
        let Self { writes, order, runs, .. } = self;
        order.clear();
        order.extend(0..writes.len());
        order.sort_unstable_by(|&a, &b| {
            (&writes[a].buffer, writes[a].offset, a).cmp(&(&writes[b].buffer, writes[b].offset, b))
        });
        runs.clear();
        let mut size = 0;
        for (n, &i) in order.iter().enumerate() {
            let w = &writes[i];
            if let Some(run) = runs.last_mut()
                && writes[order[run.writes.start]].buffer == w.buffer
                && w.offset <= run.offset + run.len
            {
                let end = run.offset + run.len;
                run.overlapping |= w.offset < end;
                let grown = (w.offset + w.len).saturating_sub(end);
                run.len += grown;
                run.writes.end = n + 1;
                size += grown;
            } else {
                runs.push(Run { offset: w.offset, at: size, len: w.len, writes: n..n + 1, overlapping: false });
                size += w.len;
            }
        }
        size
    }

    /// Lays the planned runs out in the upload, `plan`'s size long: `put`
    /// writes bytes at an offset in it.
    fn fill(&mut self, mut put: impl FnMut(usize, &[u8])) {
        let Self { data, writes, order, runs } = self;
        for run in runs.iter() {
            let members = &mut order[run.writes.clone()];
            if run.overlapping {
                // Gathered order, so later writes land over earlier ones.
                members.sort_unstable();
            }
            for &i in members.iter() {
                let w = &writes[i];
                let (at, len) = (w.at as usize, w.len as usize);
                put((run.at + w.offset - run.offset) as usize, &data[at..at + len]);
            }
        }
    }

    fn clear(&mut self) {
        self.data.clear();
        self.writes.clear();
        self.runs.clear();
    }
}

/// The buffer writes gathered since the last submission, and the staging
/// memory that uploads them.
#[derive(Debug, Default)]
pub(crate) struct Writes {
    pending: Pending<wgpu::Buffer>,
    #[cfg(not(target_arch = "wasm32"))]
    belt: Option<wgpu::util::StagingBelt>,
    /// Reused while it's big enough: writes to it are ordered after the
    /// submissions that copied from it.
    #[cfg(target_arch = "wasm32")]
    staging: Option<wgpu::Buffer>,
    #[cfg(target_arch = "wasm32")]
    upload: Vec<u8>,
}

impl Writes {
    /// Writes `data` at `offset` in `buffer` ahead of the commands submitted
    /// after the next [`encode`](Self::encode)'s, as `Queue::write_buffer`
    /// does ahead of the next submission's.
    pub(crate) fn write(&mut self, queue: &wgpu::Queue, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // Copies take 4-byte offsets and sizes: the queue reports an
        // unaligned write.
        let aligned = offset.is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT)
            && (data.len() as u64).is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT);
        if aligned && (data.len() <= GATHERED || self.pending.overlaps(buffer, offset, data.len() as u64)) {
            self.pending.gather(buffer.clone(), offset, data);
        } else {
            queue.write_buffer(buffer, offset, data);
        }
    }

    /// Uploads the gathered writes and returns the commands that copy them
    /// into place, to submit ahead of the rest, or `None` when there are
    /// none.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn encode(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<wgpu::CommandBuffer> {
        let size = wgpu::BufferSize::new(self.pending.plan())?;
        let belt = self.belt.get_or_insert_with(|| wgpu::util::StagingBelt::new(device.clone(), GATHERED as u64));
        // The belt aligns its slices for mapping, which copies' alignment divides.
        let slice = belt.allocate(size, wgpu::BufferSize::MIN);
        let Ok(mut view) = slice.get_mapped_range_mut() else {
            // A belt's slices are mapped; were one not, the queue still
            // writes them, in the order they came.
            let Pending { data, writes, .. } = &self.pending;
            for w in writes {
                queue.write_buffer(&w.buffer, w.offset, &data[w.at as usize..(w.at + w.len) as usize]);
            }
            self.pending.clear();
            return None;
        };
        self.pending.fill(|to, bytes| view.slice(to..to + bytes.len()).copy_from_slice(bytes));
        drop(view);
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Buffer writes") });
        let (staging, start) = (slice.buffer(), slice.offset());
        for run in &self.pending.runs {
            encoder.copy_buffer_to_buffer(staging, start + run.at, self.pending.target(run), run.offset, run.len);
        }
        belt.finish_and_recall_on_submit(&encoder);
        self.pending.clear();
        Some(encoder.finish())
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn encode(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<wgpu::CommandBuffer> {
        let size = self.pending.plan();
        if size == 0 {
            return None;
        }
        let upload = &mut self.upload;
        upload.clear();
        upload.resize(size as usize, 0);
        self.pending.fill(|to, bytes| upload[to..to + bytes.len()].copy_from_slice(bytes));
        let staging = match &self.staging {
            Some(staging) if staging.size() >= size => staging.clone(),
            _ => {
                let staging = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Buffer writes"),
                    size: size.next_power_of_two().max(GATHERED as u64),
                    usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.staging = Some(staging.clone());
                staging
            }
        };
        queue.write_buffer(&staging, 0, upload.as_slice());
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Buffer writes") });
        for run in &self.pending.runs {
            encoder.copy_buffer_to_buffer(&staging, run.at, self.pending.target(run), run.offset, run.len);
        }
        self.pending.clear();
        Some(encoder.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(writes: &[(u32, u64, &[u8])]) -> (Vec<(u32, u64, Vec<u8>)>, u64) {
        let mut pending = Pending::default();
        for &(b, o, d) in writes {
            pending.gather(b, o, d);
        }
        let size = pending.plan();
        let mut bytes = vec![0; size as usize];
        pending.fill(|to, b| bytes[to..to + b.len()].copy_from_slice(b));
        let runs = pending
            .runs
            .iter()
            .map(|r| {
                let at = r.at as usize;
                (*pending.target(r), r.offset, bytes[at..at + r.len as usize].to_vec())
            })
            .collect();
        (runs, size)
    }

    #[test]
    fn merges_touching_writes_into_one_copy() {
        let (runs, size) = upload(&[
            (1, 8, &[3, 3, 3, 3]),
            (0, 0, &[1, 1, 1, 1]),
            (1, 0, &[2, 2, 2, 2, 2, 2, 2, 2]),
            (0, 8, &[4, 4, 4, 4]),
        ]);
        assert_eq!(
            runs,
            [(0, 0, vec![1, 1, 1, 1]), (0, 8, vec![4, 4, 4, 4]), (1, 0, vec![2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3])]
        );
        assert_eq!(size, 20);
    }

    #[test]
    fn later_writes_win_where_they_overlap() {
        let (runs, _) = upload(&[
            (0, 4, &[1, 1, 1, 1, 1, 1, 1, 1]),
            (0, 0, &[2, 2, 2, 2, 2, 2, 2, 2]),
            (0, 8, &[3, 3, 3, 3]),
            (0, 4, &[4, 4, 4, 4]),
        ]);
        assert_eq!(runs, [(0, 0, vec![2, 2, 2, 2, 4, 4, 4, 4, 3, 3, 3, 3])]);
    }
}
