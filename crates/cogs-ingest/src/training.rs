//! Training-pair capture: every teacher call is recorded as a JSONL line, and
//! each run writes a manifest tying records to the files they produced. At
//! distill time the manifest is the acceptance signal — the *surviving* file
//! content (post human review) is paired with the original recorded inputs.
//!
//! Records live under `<training_dir>/runs/<run_id>.jsonl` + `.meta.json`.
//! Unlike the graph DB this data is NOT regenerable — teacher calls cost
//! money — so it lives in the state dir but should be treated as precious.

use std::cell::{Cell, RefCell};
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use cogs_llm::{extract_json, repair_truncated_json, ChatProvider, CompletionParams, Message};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Extract,
    ExtractChunk,
    ExtractMerge,
    SuggestLinks,
    PageUpdate,
    Contradiction,
}

/// One recorded teacher call.
#[derive(Debug, Serialize, Deserialize)]
pub struct TrainingRecord {
    pub run_id: String,
    pub seq: u32,
    pub task: TaskKind,
    pub provider: String,
    pub model: String,
    pub created: String,
    pub messages: Vec<Message>,
    pub raw_output: String,
    pub parsed_ok: bool,
    /// Call-site context (raw_path, page_id, ...).
    pub meta: serde_json::Value,
}

/// Ties a run's records to the files it wrote — the anchor for
/// `cogs distill --from-runs` acceptance pairing.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub created: String,
    pub raw_rel_path: String,
    pub raw_body_hash: String,
    pub provider: String,
    pub model: String,
    pub writes: Vec<WriteRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRecord {
    pub rel_path: String,
    pub kind: WriteKind,
    /// Exact `## … (date ingest)` heading text for appended sections.
    pub section_heading: Option<String>,
    /// sha256 of what ingest wrote (whole file for Created, section for
    /// Appended) — lets distill tell "accepted verbatim" from "human-edited".
    pub content_hash: String,
    /// Which training record produced this write, if one did.
    pub seq: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    Created,
    Appended,
    FmEdited,
}

pub struct TrainingRecorder {
    runs_dir: PathBuf,
    run_id: String,
    provider: String,
    model: String,
    seq: Cell<u32>,
    file: RefCell<Option<std::fs::File>>,
}

impl TrainingRecorder {
    pub fn new(runs_dir: PathBuf, run_id: &str, provider: &str, model: &str) -> Self {
        Self {
            runs_dir,
            run_id: run_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            seq: Cell::new(0),
            file: RefCell::new(None),
        }
    }

    pub fn count(&self) -> u32 {
        self.seq.get()
    }

    fn record(
        &self,
        task: TaskKind,
        meta: &serde_json::Value,
        messages: &[Message],
        raw_output: &str,
        parsed_ok: bool,
    ) -> Result<u32> {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        let rec = TrainingRecord {
            run_id: self.run_id.clone(),
            seq,
            task,
            provider: self.provider.clone(),
            model: self.model.clone(),
            created: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            messages: messages.to_vec(),
            raw_output: raw_output.to_string(),
            parsed_ok,
            meta: meta.clone(),
        };
        let mut guard = self.file.borrow_mut();
        if guard.is_none() {
            std::fs::create_dir_all(&self.runs_dir)?;
            let path = self.runs_dir.join(format!("{}.jsonl", self.run_id));
            *guard = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("opening {}", path.display()))?,
            );
        }
        let f = guard.as_mut().unwrap();
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        Ok(seq)
    }

    pub fn finish(&self, manifest: &RunManifest) -> Result<()> {
        std::fs::create_dir_all(&self.runs_dir)?;
        let path = self.runs_dir.join(format!("{}.meta.json", self.run_id));
        std::fs::write(&path, serde_json::to_vec_pretty(manifest)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// The teacher: a chat provider plus optional capture. All pipeline LLM calls
/// go through `call`, which completes, records (even on parse failure), and
/// retries once when the reply wasn't valid JSON — local models occasionally
/// wrap or truncate.
pub struct Teacher<'a> {
    chat: &'a dyn ChatProvider,
    recorder: Option<&'a TrainingRecorder>,
}

impl<'a> Teacher<'a> {
    pub fn new(chat: &'a dyn ChatProvider, recorder: Option<&'a TrainingRecorder>) -> Self {
        Self { chat, recorder }
    }

    /// Complete + parse as T, recording the exchange. Returns the seq of the
    /// successful record (0 when capture is off) alongside the value.
    pub fn call<T: serde::de::DeserializeOwned>(
        &self,
        task: TaskKind,
        meta: serde_json::Value,
        messages: &[Message],
        params: &CompletionParams,
    ) -> Result<(T, u32)> {
        let mut params = params.clone();
        params.json = true;

        let mut msgs: Vec<Message> = messages.to_vec();
        for attempt in 0..2 {
            let raw = self.chat.complete(&msgs, &params)?;
            // Balanced JSON if present; else salvage a max_tokens truncation.
            let candidate: Option<String> = extract_json(&raw)
                .map(str::to_string)
                .or_else(|| repair_truncated_json(&raw));
            let parsed: Result<T> = candidate
                .context("no JSON object/array in model reply")
                .and_then(|s| {
                    serde_json::from_str(&s)
                        .context("model reply was not the expected JSON shape")
                });
            let seq = match &self.recorder {
                Some(r) => r.record(task, &meta, &msgs, &raw, parsed.is_ok())?,
                None => 0,
            };
            match parsed {
                Ok(v) => return Ok((v, seq)),
                Err(e) if attempt == 0 => {
                    tracing::warn!("teacher reply unparseable, retrying once: {e:#}");
                    msgs.push(Message::assistant(raw));
                    msgs.push(Message::user(
                        "Your previous reply was not the required JSON. Reply again with \
                         ONLY the JSON value, no prose, matching the schema exactly.",
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop returns on success or second failure")
    }
}
