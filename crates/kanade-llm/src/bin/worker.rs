// SPDX-License-Identifier: MIT
//! Isolated inference worker. It speaks only the private frame protocol on its
//! standard streams, writes no diagnostics and stores nothing.
#[path = "worker/char_prior.rs"]
mod char_prior;
use kanade_llm::{
    Job,
    frame::{self, Fault, Loaded, Reply, Request},
    limits,
    profile::{self, Profile},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    time::{Duration, Instant},
};

fn main() {
    let mut input = io::stdin().lock();
    let Some(mut output) = private_output() else {
        return;
    };
    // Without its restrictions the worker does not serve anything.
    if !isolate() {
        return;
    }
    if send(
        &mut output,
        &Reply::Hello {
            protocol: frame::PROTOCOL,
            runtime: profile::RUNTIME.into(),
            accelerators: llama::accelerators(),
        },
    )
    .is_err()
    {
        return;
    }
    // The loaded model together with the one profile registered for it.
    let mut runtime: Option<(llama::Runtime, &'static Profile)> = None;
    // A closed pipe means the parent is gone: stop immediately.
    while let Ok(body) = frame::read(&mut input) {
        let Some(request) = Request::decode(&body) else {
            let _ = send(
                &mut output,
                &Reply::Error {
                    id: 0,
                    fault: Fault::Malformed,
                },
            );
            break;
        };
        let reply = match request {
            Request::Shutdown => break,
            Request::Load {
                path,
                threads,
                model,
            } => {
                if runtime.is_some() {
                    Reply::Error {
                        id: 0,
                        fault: Fault::AlreadyLoaded,
                    }
                } else {
                    match load(&path, threads, &model) {
                        Ok((loaded, profile, report)) => {
                            runtime = Some((loaded, profile));
                            Reply::Loaded(report)
                        }
                        Err(fault) => Reply::Error { id: 0, fault },
                    }
                }
            }
            Request::Score {
                id,
                budget_ms,
                job,
                sentence,
            } => match runtime.as_mut() {
                None => Reply::Error {
                    id,
                    fault: Fault::NotLoaded,
                },
                Some((runtime, profile)) => {
                    let budget = Duration::from_millis(budget_ms.min(limits::BUDGET_MS) as u64);
                    let indices = if sentence && job.candidates.len() > limits::CANDIDATES {
                        runtime
                            .prior
                            .as_ref()
                            .map(|p| p.shortlist(&job))
                            .unwrap_or_else(|| (0..limits::CANDIDATES).collect())
                    } else {
                        (0..job.candidates.len()).collect()
                    };
                    let mut selected = job.clone();
                    selected.candidates =
                        indices.iter().map(|&i| job.candidates[i].clone()).collect();
                    match score(runtime, profile, &selected, budget) {
                        Ok((mut local, tokens)) => {
                            if sentence && let Some(prior) = &runtime.prior {
                                prior.combine(&selected, &mut local);
                            }
                            let mut scores = vec![None; job.candidates.len()];
                            for (index, value) in indices.into_iter().zip(local) {
                                scores[index] = value;
                            }
                            Reply::Scores { id, scores, tokens }
                        }
                        Err(fault) => Reply::Error { id, fault },
                    }
                }
            },
            Request::Tokenize { id, texts } => match runtime.as_ref() {
                None => Reply::Error {
                    id,
                    fault: Fault::NotLoaded,
                },
                Some((runtime, _)) => {
                    match texts
                        .iter()
                        .map(|text| {
                            runtime
                                .tokenize(text)
                                .map(|ids| ids.map(|ids| ids.iter().map(|t| *t as u32).collect()))
                        })
                        .collect::<Result<Vec<_>, _>>()
                    {
                        Ok(ids) => Reply::Tokens { id, ids },
                        Err(fault) => Reply::Error { id, fault },
                    }
                }
            },
        };
        if send(&mut output, &reply).is_err() {
            break;
        }
    }
}

fn send(output: &mut File, reply: &Reply) -> io::Result<()> {
    frame::write(output, &reply.encode())
}

/// Keeps the frame channel private: anything the runtime prints to the standard
/// output or error stream goes to the null device instead of the parent.
#[cfg(unix)]
fn private_output() -> Option<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let null = File::options().write(true).open("/dev/null").ok()?;
    // SAFETY: the descriptors are valid for the whole process and the duplicate
    // is owned by exactly one `File`.
    unsafe {
        let channel = libc::dup(1);
        if channel < 0 || libc::dup2(null.as_raw_fd(), 1) < 0 || libc::dup2(null.as_raw_fd(), 2) < 0
        {
            return None;
        }
        libc::fcntl(channel, libc::F_SETFD, libc::FD_CLOEXEC);
        Some(File::from_raw_fd(channel))
    }
}
#[cfg(windows)]
fn private_output() -> Option<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // The parent already sends the error stream to the null device.
    // SAFETY: the standard output handle is valid for the whole process and
    // this `File` is its only user.
    Some(unsafe { File::from_raw_handle(io::stdout().as_raw_handle()) })
}

/// Drops what inference never needs: the network, writing files and starting
/// processes. Reading the model and using the already open pipes still work.
#[cfg(target_os = "macos")]
fn isolate() -> bool {
    use std::ffi::{c_char, c_int};
    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, error: *mut *mut c_char) -> c_int;
    }
    let profile = c"(version 1)(allow default)(deny network*)(deny file-write*)(deny process-fork)(deny process-exec)";
    let mut error = std::ptr::null_mut();
    // SAFETY: a NUL-terminated profile and a valid out-pointer. The message
    // buffer of a failed call is deliberately neither read nor freed.
    unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) == 0 }
}
/// Other systems rely on the private pipes and the cleared environment only.
#[cfg(not(target_os = "macos"))]
fn isolate() -> bool {
    true
}

/// Loads `path` as the registered model `id`. The profile comes from this
/// build's registry and from nowhere else; a file or vocabulary that is not
/// the registered one is refused.
fn load(
    path: &str,
    threads: u8,
    id: &str,
) -> Result<(llama::Runtime, &'static Profile, Loaded), Fault> {
    let model = profile::model(id).ok_or(Fault::UnknownModel)?;
    let profile = model.profile;
    if !profile.well_formed() {
        return Err(Fault::Internal);
    }
    let identity = |file: &File| {
        let meta = file.metadata().map_err(|_| Fault::ModelUnreadable)?;
        if !meta.is_file() {
            return Err(Fault::ModelUnreadable);
        }
        Ok((meta.len(), meta.modified().ok()))
    };
    let mut file = File::open(path).map_err(|_| Fault::ModelUnreadable)?;
    let before = identity(&file)?;
    if before.0 != model.size {
        return Err(Fault::ProfileMismatch);
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let len = file.read(&mut buffer).map_err(|_| Fault::ModelUnreadable)?;
        if len == 0 {
            break;
        }
        size += len as u64;
        digest.update(&buffer[..len]);
    }
    let model_sha256: [u8; 32] = digest.finalize().into();
    // Only the registered file reaches the runtime's parser.
    if size != model.size || profile::hex(&model_sha256) != model.sha256 {
        return Err(Fault::ProfileMismatch);
    }
    let threads = threads.clamp(1, limits::THREADS);
    let runtime = llama::Runtime::load(path, threads, profile, &model.preview)?;
    // The runtime reads the same path again. Refuse a file replaced meanwhile.
    let after = identity(&File::open(path).map_err(|_| Fault::ModelUnreadable)?)?;
    if before != after || before.0 != size {
        return Err(Fault::ModelChanged);
    }
    let vocab_sha256 = runtime.vocab_digest();
    if runtime.vocab_size() as u32 != profile.vocab_size
        || profile::hex(&vocab_sha256) != profile.vocab_sha256
    {
        return Err(Fault::ProfileMismatch);
    }
    let report = Loaded {
        model_sha256,
        model_size: size,
        vocab_sha256,
        vocab_size: runtime.vocab_size() as u32,
        profile_sha256: profile.digest(),
        start: profile.start,
        end: runtime.end() as u32,
        context_tokens: limits::CONTEXT_TOKENS,
        threads,
    };
    Ok((runtime, profile, report))
}

fn score(
    runtime: &mut llama::Runtime,
    profile: &Profile,
    job: &Job,
    budget: Duration,
) -> Result<(Vec<Option<f32>>, u32), Fault> {
    let prompt = &profile.prompt;
    // Text that contains a marker of this profile is refused as a whole.
    // Nothing is removed from a reading or a surface.
    if !prompt.admits(job) {
        return Err(Fault::Reserved);
    }
    let deadline = Instant::now() + budget;
    let unscored = || Ok((vec![None; job.candidates.len()], 0));
    // Keep the longest trailing context the tokenizer reproduces exactly.
    let normalized = prompt.normalize(&job.context);
    let mut from = normalized.len();
    for (index, c) in normalized.char_indices().rev() {
        let mut bytes = [0; 4];
        if runtime.tokenize(c.encode_utf8(&mut bytes))?.is_none() {
            break;
        }
        from = index;
    }
    let context = &normalized[from..];
    if context.is_empty() && !job.context.is_empty() {
        return unscored();
    }
    // A start token only where the profile names one.
    let mut conditioning = Vec::new();
    conditioning.extend(profile.start.map(|start| start as i32));
    conditioning.extend(
        runtime
            .tokenize(&prompt.conditioning(context, &prompt.reading(&job.reading)))?
            .ok_or(Fault::Tokenizer)?,
    );
    let Some(prefix) = runtime.tokenize(&prompt.normalize(&job.prefix))? else {
        return unscored();
    };
    conditioning.extend(prefix);
    if conditioning.len() > limits::PROMPT_TOKENS {
        return Err(Fault::TooLong);
    }
    let mut continuations = Vec::with_capacity(job.candidates.len());
    for candidate in &job.candidates {
        let mut text = prompt.normalize(candidate);
        text.push_str(&prompt.normalize(&job.suffix));
        continuations.push(
            runtime
                .tokenize(&text)?
                .filter(|ids| !ids.is_empty() && ids.len() < limits::CONTINUATION_TOKENS),
        );
    }
    runtime.score(&conditioning, &continuations, deadline)
}

mod llama {
    use super::*;
    use llama_cpp_sys_2 as sys;
    use std::{
        ffi::{CString, c_char, c_void},
        ptr,
        sync::atomic::{AtomicU64, Ordering},
    };

    unsafe extern "C" fn silent(_: sys::ggml_log_level, _: *const c_char, _: *mut c_void) {}

    struct Clock {
        base: Instant,
        /// Microseconds after `base`; `u64::MAX` means no deadline.
        limit: AtomicU64,
    }
    unsafe extern "C" fn expired(data: *mut c_void) -> bool {
        // SAFETY: `data` is the boxed `Clock` owned by the runtime, which
        // outlives its context.
        let clock = unsafe { &*(data as *const Clock) };
        clock.base.elapsed().as_micros() as u64 >= clock.limit.load(Ordering::Relaxed)
    }

    /// Silences the runtime and counts compute devices other than the CPU.
    /// The pinned build has none; the parent refuses a worker that reports any.
    pub fn accelerators() -> u8 {
        // SAFETY: plain C calls; device handles are owned by the registry.
        unsafe {
            sys::llama_log_set(Some(silent), ptr::null_mut());
            sys::llama_backend_init();
            (0..sys::ggml_backend_dev_count())
                .filter(|index| {
                    sys::ggml_backend_dev_type(sys::ggml_backend_dev_get(*index))
                        != sys::GGML_BACKEND_DEVICE_TYPE_CPU
                })
                .count()
                .min(255) as u8
        }
    }

    pub struct Runtime {
        pub(super) prior: Option<char_prior::Prior>,
        model: *mut sys::llama_model,
        context: *mut sys::llama_context,
        vocab: *const sys::llama_vocab,
        batch: sys::llama_batch,
        clock: Box<Clock>,
        vocab_size: usize,
        end: i32,
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            // SAFETY: each pointer was returned by the matching constructor and
            // is released exactly once.
            unsafe {
                sys::llama_batch_free(self.batch);
                sys::llama_free(self.context);
                sys::llama_model_free(self.model);
            }
        }
    }
    impl Runtime {
        /// Loads the file with the tokenizer overrides and the end token of
        /// `profile`.
        pub fn load(
            path: &str,
            threads: u8,
            profile: &Profile,
            preview: &profile::Preview,
        ) -> Result<Self, Fault> {
            let path = CString::new(path).map_err(|_| Fault::ModelUnreadable)?;
            // SAFETY: plain C calls with valid, NUL-terminated arguments. The
            // override array ends with an empty key as the API requires.
            unsafe {
                // Only the CPU device, as a null-terminated list: nothing else
                // may be initialized even if another backend were present.
                let cpu = sys::ggml_backend_dev_by_type(sys::GGML_BACKEND_DEVICE_TYPE_CPU);
                if cpu.is_null() {
                    return Err(Fault::Internal);
                }
                let mut devices = [cpu, ptr::null_mut()];
                // One more entry than a profile may have: the terminator.
                let mut overrides: [sys::llama_model_kv_override; profile::MAX_OVERRIDES + 1] =
                    std::mem::zeroed();
                let wanted = profile.tokenizer.overrides;
                if wanted.len() > profile::MAX_OVERRIDES {
                    return Err(Fault::Internal);
                }
                for (slot, (key, value)) in overrides.iter_mut().zip(wanted) {
                    if !copy(&mut slot.key, key) || !copy(&mut slot.__bindgen_anon_1.val_str, value)
                    {
                        return Err(Fault::Internal);
                    }
                    slot.tag = sys::LLAMA_KV_OVERRIDE_TYPE_STR;
                }
                let mut params = sys::llama_model_default_params();
                params.n_gpu_layers = 0;
                params.devices = devices.as_mut_ptr();
                if !wanted.is_empty() {
                    params.kv_overrides = overrides.as_ptr();
                }
                let model = sys::llama_model_load_from_file(path.as_ptr(), params);
                if model.is_null() {
                    return Err(Fault::ModelRejected);
                }
                let clock = Box::new(Clock {
                    base: Instant::now(),
                    limit: AtomicU64::new(u64::MAX),
                });
                let mut params = sys::llama_context_default_params();
                params.n_ctx = limits::CONTEXT_TOKENS;
                params.n_batch = limits::CONTEXT_TOKENS;
                params.n_ubatch = limits::CONTEXT_TOKENS;
                params.n_seq_max = 1;
                params.n_threads = threads as i32;
                params.n_threads_batch = threads as i32;
                params.no_perf = true;
                params.embeddings = false;
                params.offload_kqv = false;
                params.op_offload = false;
                params.abort_callback = Some(expired);
                params.abort_callback_data = &*clock as *const Clock as *mut c_void;
                let context = sys::llama_init_from_model(model, params);
                if context.is_null() {
                    sys::llama_model_free(model);
                    return Err(Fault::ModelRejected);
                }
                let vocab = sys::llama_model_get_vocab(model);
                let batch = sys::llama_batch_init(limits::CONTEXT_TOKENS as i32, 0, 1);
                let mut runtime = Self {
                    prior: None,
                    model,
                    context,
                    vocab,
                    batch,
                    clock,
                    vocab_size: sys::llama_vocab_n_tokens(vocab).max(0) as usize,
                    end: profile.end as i32,
                };
                if runtime.vocab_size == 0
                    || runtime.end as usize >= runtime.vocab_size
                    || profile
                        .start
                        .is_some_and(|start| start as usize >= runtime.vocab_size)
                    || sys::llama_n_ctx(context) < limits::CONTEXT_TOKENS
                {
                    return Err(Fault::ModelRejected);
                }
                let key = c"tsumugi.char_prior.v1".as_ptr();
                // SAFETY: the model is live and the first call only queries the length.
                let size = sys::llama_model_meta_val_str(model, key, ptr::null_mut(), 0);
                if size >= 0 {
                    if size as usize > char_prior::MAX_BYTES {
                        return Err(Fault::ModelRejected);
                    }
                    let mut encoded = vec![0u8; size as usize + 1];
                    let written = sys::llama_model_meta_val_str(
                        model,
                        key,
                        encoded.as_mut_ptr().cast(),
                        encoded.len(),
                    );
                    if written != size {
                        return Err(Fault::ModelRejected);
                    }
                    runtime.prior = Some(char_prior::Prior::decode(
                        &encoded[..size as usize],
                        preview,
                    )?);
                }
                Ok(runtime)
            }
        }
        pub fn vocab_size(&self) -> usize {
            self.vocab_size
        }
        pub fn end(&self) -> i32 {
            self.end
        }
        pub fn vocab_digest(&self) -> [u8; 32] {
            let mut digest = Sha256::new();
            digest.update((self.vocab_size as u32).to_le_bytes());
            for token in 0..self.vocab_size as i32 {
                // SAFETY: the token is in range and the vocabulary owns the text.
                let text = unsafe {
                    let text = sys::llama_vocab_get_text(self.vocab, token);
                    if text.is_null() {
                        &[][..]
                    } else {
                        std::ffi::CStr::from_ptr(text).to_bytes()
                    }
                };
                digest.update(text);
                digest.update([0]);
            }
            digest.finalize().into()
        }
        /// Tokenizes without special-token parsing. Returns `None` unless the
        /// tokens reproduce the text byte for byte.
        pub fn tokenize(&self, text: &str) -> Result<Option<Vec<i32>>, Fault> {
            if text.is_empty() {
                return Ok(Some(Vec::new()));
            }
            if text.len() > limits::TEXT_BYTES * 4 {
                return Err(Fault::TooLong);
            }
            let mut tokens = vec![0i32; text.len() + 8];
            let mut bytes = Vec::with_capacity(text.len());
            // SAFETY: the buffers are valid for the stated lengths.
            unsafe {
                let count = sys::llama_tokenize(
                    self.vocab,
                    text.as_ptr() as *const c_char,
                    text.len() as i32,
                    tokens.as_mut_ptr(),
                    tokens.len() as i32,
                    false,
                    false,
                );
                if count < 0 {
                    return Err(Fault::Tokenizer);
                }
                tokens.truncate(count as usize);
                for token in &tokens {
                    let mut piece = [0 as c_char; 64];
                    let len = sys::llama_token_to_piece(
                        self.vocab,
                        *token,
                        piece.as_mut_ptr(),
                        piece.len() as i32,
                        0,
                        false,
                    );
                    if len < 0 {
                        return Err(Fault::Tokenizer);
                    }
                    bytes.extend(piece[..len as usize].iter().map(|b| *b as u8));
                }
            }
            if tokens
                .iter()
                .any(|t| *t < 0 || *t as usize >= self.vocab_size)
            {
                return Err(Fault::Tokenizer);
            }
            Ok((bytes == text.as_bytes()).then_some(tokens))
        }
        /// Sum of log probabilities of each continuation and the end token.
        pub fn score(
            &mut self,
            conditioning: &[i32],
            continuations: &[Option<Vec<i32>>],
            deadline: Instant,
        ) -> Result<(Vec<Option<f32>>, u32), Fault> {
            let limit = deadline
                .saturating_duration_since(self.clock.base)
                .as_micros() as u64;
            self.clock.limit.store(limit, Ordering::Relaxed);
            let result = self.score_before_deadline(conditioning, continuations);
            self.clock.limit.store(u64::MAX, Ordering::Relaxed);
            // No later request reuses this context. Clear the KV data and
            // input batch after success, refusal and aborted decoding alike.
            // SAFETY: the live context owns its memory and the batch was
            // allocated for CONTEXT_TOKENS entries in Runtime::load.
            unsafe {
                sys::llama_memory_clear(sys::llama_get_memory(self.context), true);
                std::slice::from_raw_parts_mut(self.batch.token, limits::CONTEXT_TOKENS as usize)
                    .fill(0);
                self.batch.n_tokens = 0;
            }
            match result {
                Err(Fault::Decode) if Instant::now() >= deadline => Err(Fault::Deadline),
                other => other,
            }
        }
        fn score_before_deadline(
            &mut self,
            conditioning: &[i32],
            continuations: &[Option<Vec<i32>>],
        ) -> Result<(Vec<Option<f32>>, u32), Fault> {
            if conditioning.is_empty() || conditioning.len() > limits::PROMPT_TOKENS {
                return Err(Fault::TooLong);
            }
            let base = conditioning.len();
            let mut evaluated = base as u32;
            // SAFETY: the memory handle belongs to the live context.
            unsafe { sys::llama_memory_clear(sys::llama_get_memory(self.context), true) };
            self.decode(conditioning, 0, false)?;
            let first = self.log_probabilities(-1)?;
            let mut scores = vec![None; continuations.len()];
            let mut groups = std::collections::BTreeMap::<i32, Vec<(usize, &[i32])>>::new();
            for (i, tokens) in continuations.iter().enumerate() {
                if let Some(t) = tokens
                    && (1..limits::CONTINUATION_TOKENS).contains(&t.len())
                {
                    groups.entry(t[0]).or_default().push((i, t));
                }
            }
            for group in groups.values() {
                let reference = group[0].1;
                let common = if group.len() > 1 && continuations.len() > limits::CANDIDATES {
                    group[1..].iter().fold(reference.len(), |n, (_, t)| {
                        reference[..n]
                            .iter()
                            .zip(*t)
                            .take_while(|(a, b)| a == b)
                            .count()
                    })
                } else {
                    0
                };
                let mut prefix_sum = 0.0;
                let mut branch = &first;
                let distribution;
                if common >= 2 {
                    self.trim(base)?;
                    self.decode(&reference[..common], base, true)?;
                    evaluated += common as u32;
                    prefix_sum = first[reference[0] as usize] as f64;
                    for (i, t) in reference[1..common].iter().enumerate() {
                        prefix_sum += self.log_probability(i as i32, *t)? as f64;
                    }
                    distribution = self.log_probabilities(-1)?;
                    branch = &distribution;
                }
                let shared = if common >= 2 { common } else { 0 };
                for &(i, tokens) in group {
                    let tail = &tokens[shared..];
                    let end = self.end;
                    let mut total = prefix_sum;
                    if tail.is_empty() {
                        total += branch[end as usize] as f64;
                    } else {
                        self.trim(base + shared)?;
                        self.decode(tail, base + shared, true)?;
                        evaluated += tail.len() as u32;
                        total += branch[tail[0] as usize] as f64;
                        for (j, t) in tail[1..].iter().chain(std::iter::once(&end)).enumerate() {
                            total += self.log_probability(j as i32, *t)? as f64;
                        }
                    }
                    let total = total as f32;
                    if !total.is_finite() || total > 0.0 {
                        return Err(Fault::Internal);
                    }
                    scores[i] = Some(total);
                }
            }
            Ok((scores, evaluated))
        }
        fn trim(&mut self, position: usize) -> Result<(), Fault> {
            // SAFETY: the context owns its KV memory. Sequence zero is the sole active sequence.
            if unsafe {
                sys::llama_memory_seq_rm(
                    sys::llama_get_memory(self.context),
                    0,
                    position as i32,
                    -1,
                )
            } {
                Ok(())
            } else {
                Err(Fault::Decode)
            }
        }
        fn decode(
            &mut self,
            tokens: &[i32],
            position: usize,
            all_logits: bool,
        ) -> Result<(), Fault> {
            if position + tokens.len() > limits::CONTEXT_TOKENS as usize {
                return Err(Fault::TooLong);
            }
            // SAFETY: the batch was allocated for `CONTEXT_TOKENS` entries with
            // one sequence id each, and `tokens` fits as checked above.
            unsafe {
                for (index, token) in tokens.iter().enumerate() {
                    *self.batch.token.add(index) = *token;
                    *self.batch.pos.add(index) = (position + index) as i32;
                    *self.batch.n_seq_id.add(index) = 1;
                    *(*self.batch.seq_id.add(index)) = 0;
                    *self.batch.logits.add(index) = (all_logits || index + 1 == tokens.len()) as i8;
                }
                self.batch.n_tokens = tokens.len() as i32;
                if sys::llama_decode(self.context, self.batch) != 0 {
                    return Err(Fault::Decode);
                }
            }
            Ok(())
        }
        fn logits(&mut self, index: i32) -> Result<&[f32], Fault> {
            // SAFETY: the runtime returns a pointer to `vocab_size` values for a
            // batch position that requested logits, or null.
            unsafe {
                let logits = sys::llama_get_logits_ith(self.context, index);
                if logits.is_null() {
                    return Err(Fault::Decode);
                }
                Ok(std::slice::from_raw_parts(logits, self.vocab_size))
            }
        }
        fn log_probabilities(&mut self, index: i32) -> Result<Vec<f32>, Fault> {
            let logits = self.logits(index)?;
            let norm = log_sum_exp(logits);
            Ok(logits.iter().map(|l| (*l as f64 - norm) as f32).collect())
        }
        fn log_probability(&mut self, index: i32, token: i32) -> Result<f32, Fault> {
            let logits = self.logits(index)?;
            Ok((logits[token as usize] as f64 - log_sum_exp(logits)) as f32)
        }
    }
    fn log_sum_exp(logits: &[f32]) -> f64 {
        let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
        max + logits
            .iter()
            .map(|l| (*l as f64 - max).exp())
            .sum::<f64>()
            .ln()
    }
    /// Copies a NUL-terminated string; refuses one that does not fit whole.
    fn copy<const N: usize>(target: &mut [c_char; N], value: &str) -> bool {
        if value.len() >= N || value.bytes().any(|byte| byte == 0) {
            return false;
        }
        for (slot, byte) in target.iter_mut().zip(value.bytes()) {
            *slot = byte as c_char;
        }
        true
    }
}
