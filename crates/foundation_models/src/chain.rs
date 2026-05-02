// SPDX-License-Identifier: AGPL-3.0-only
//
// Chained execution loop on top of Foundation Models (PDX-15).
//
// # What this is
//
// FM's session has a small, finite context window relative to a frontier
// LLM. To drive a multi-step task we can't replay the whole conversation
// history every step — we'd blow the window after a handful of tool
// invocations. Instead this module owns a *plan-step-execute-result* loop
// that:
//
//   1. asks FM, given the goal, which tool to call next (a `ToolChoice`
//      `@Generable` enum from PDX-14);
//   2. invokes that tool through the orchestrator's MCP forwarder
//      (PDX-105) and captures the result;
//   3. asks FM, given the *minimised* prompt (goal + last result + a
//      compressed summary of prior steps), to either pick the next tool
//      or finish with a final answer (`ContinueOrFinish`);
//   4. repeats until `Finish`, the step bound is hit, or a capability
//      miss escalates the task to a more capable provider.
//
// Session state lives **outside** the model — see [`SessionState`] —
// because re-feeding it as plain text every step is the only way to keep
// FM's per-call prompt small and stable.
//
// # Why no `tokio` here
//
// Per PDX-13's notes the FM bridge is a blocking API and the call site
// (PDX-16's router) wraps it in `spawn_blocking`. The loop driver
// therefore stays synchronous: you give it a `FmCompletion` impl and a
// `ToolInvoker` impl, and it runs the state machine on the calling
// thread. Tests substitute mock impls and exercise the loop without
// touching the real FM runtime.
//
// # Why this is macOS-only
//
// The whole crate is gated on `cfg(target_os = "macos")` for the FFI
// bridge. The chain loop has no Apple-specific code per se, but it only
// makes sense as a driver for the FM session and we don't want to grow
// a no-op stub on Linux. PDX-16 will register the FM agent on macOS only
// for the same reason.
//
// # What PDX-16 does next
//
// PDX-16 wires this loop in as the `run()` impl of the FoundationModels
// agent in the persistent Router. Concretely:
//
//   * Build a `ChainConfig` with the registered MCP tools (translated
//     once via [`crate::generable::translate_tools`]).
//   * Wrap [`crate::complete`] in an `FmCompletion` impl.
//   * Wrap the orchestrator's `McpForwarder` in a `ToolInvoker` impl.
//   * Call [`run_chain`] inside a `spawn_blocking` task.
//   * On `LoopOutcome::Escalate` the router reroutes the task to the
//     next-capable provider in `Role::Coding` order (Claude Code, Codex).
//   * On `LoopOutcome::StepBoundExceeded` the router surfaces the
//     accumulated `SessionState` to the user with a "FM stalled" note.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::generable::TranslationError;

/// Default upper bound on chain steps when [`ChainConfig::max_steps`] is
/// not overridden. Picked to match the rule of thumb that on-device FM
/// rarely makes useful progress past about a dozen tool calls before the
/// per-step prompt drift outweighs the structured-output benefit; if a
/// real workload needs more, the orchestrator should be reconsidering FM
/// for the task in the first place.
pub const DEFAULT_MAX_STEPS: usize = 10;

/// Soft cap on the rendered prompt length, in characters. Foundation
/// Models doesn't expose a tokenizer so we use byte/char count as a
/// cheap, deterministic proxy: ~4 characters per token gives us roughly
/// a 1k-token budget, which keeps us comfortably inside FM's context
/// window with room for the structured-output schema preamble.
///
/// The chain renderer trims the compressed summary first when the
/// budget is exceeded; if a single step's result alone is larger than
/// the budget we still emit it (truncated with a marker) rather than
/// drop the most recent observation, since that's the signal the model
/// most needs to plan the next step.
pub const PROMPT_BUDGET_CHARS: usize = 4_000;

/// Dependency-injection seam for the FM completion call. In production
/// this is a thin wrapper around [`crate::complete`]; in tests it's a
/// scripted mock that returns canned responses. Keeping the loop
/// generic over this trait is what lets us unit-test the state machine
/// without standing up an actual Foundation Models session.
///
/// The `expected` parameter tells the impl what shape of structured
/// output the loop expects back (`ToolChoice` vs `ContinueOrFinish`).
/// Production impls embed this in the prompt as a `@Generable` schema
/// preamble; mocks branch on it to script different responses for the
/// initial step vs follow-ups.
pub trait FmCompletion {
    /// Run a single FM completion. The returned string is the model's
    /// raw output — the loop parses it into the structured response
    /// shape downstream. Errors propagate verbatim.
    fn complete(
        &mut self,
        prompt: &str,
        expected: ExpectedResponse,
    ) -> Result<String, ChainError>;
}

/// Adapter that turns an `FnMut` closure into an [`FmCompletion`].
/// We expose this as a wrapper struct rather than a blanket
/// `impl<F: FnMut> FmCompletion for F` because the latter conflicts
/// with the `&mut T` forwarding impl below — any `&mut T` is itself
/// `FnMut`-coercible from Rust's perspective and the trait selector
/// can't tell which path the user wants. Tests use either
/// [`FmFn::new`] for closure-style mocks or a direct `impl
/// FmCompletion` for stateful ones.
pub struct FmFn<F>(F);

impl<F> FmFn<F>
where
    F: FnMut(&str, ExpectedResponse) -> Result<String, ChainError>,
{
    /// Wrap a closure of the right shape into an [`FmCompletion`].
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> FmCompletion for FmFn<F>
where
    F: FnMut(&str, ExpectedResponse) -> Result<String, ChainError>,
{
    fn complete(
        &mut self,
        prompt: &str,
        expected: ExpectedResponse,
    ) -> Result<String, ChainError> {
        (self.0)(prompt, expected)
    }
}

/// Blanket forwarding impl so callers can pass `&mut some_fm` to
/// [`run_chain`] without consuming a shared completion handle. Tests
/// rely on this to inspect the FM mock after the loop returns.
impl<T: FmCompletion + ?Sized> FmCompletion for &mut T {
    fn complete(
        &mut self,
        prompt: &str,
        expected: ExpectedResponse,
    ) -> Result<String, ChainError> {
        (**self).complete(prompt, expected)
    }
}

/// Which structured-output shape the loop is asking the model for on
/// this step. Production callers translate this into the right
/// `@Generable` schema in the prompt preamble; the chain loop itself
/// only uses it to pick the right parser for the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedResponse {
    /// First step: model picks a tool from the registered set.
    /// Response shape: `{ "tool": "<tool_name>", "args": {...},
    /// "reasoning": "<one-liner>" }`.
    ToolChoice,
    /// Follow-up step: model either picks the next tool or finishes
    /// with a final answer.
    /// Response shape: either `{ "continue": { "tool": ..., "args": ...,
    /// "reasoning": ... } }` or `{ "finish": { "answer": ... } }`.
    ContinueOrFinish,
}

/// Dependency-injection seam for tool invocation. PDX-16 wires this to
/// the orchestrator's `McpForwarder`; tests stub it with a closure that
/// returns canned tool results.
///
/// The `args` parameter is the raw JSON the model produced for the
/// tool's input schema. We don't validate it here — the MCP server
/// itself will reject malformed args, and surfacing those errors as
/// step failures is exactly the signal we want the loop to learn from.
pub trait ToolInvoker {
    /// Invoke `tool_name` with `args`. Returns the tool's result on
    /// success, or [`ChainError::ToolFailed`] (which the loop treats
    /// as a recoverable error up to [`ChainConfig::max_consecutive_failures`]).
    fn invoke(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ChainError>;
}

/// Closure → [`ToolInvoker`] adapter, mirroring [`FmFn`].
pub struct ToolFn<F>(F);

impl<F> ToolFn<F>
where
    F: FnMut(&str, &serde_json::Value) -> Result<serde_json::Value, ChainError>,
{
    /// Wrap a closure of the right shape into a [`ToolInvoker`].
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> ToolInvoker for ToolFn<F>
where
    F: FnMut(&str, &serde_json::Value) -> Result<serde_json::Value, ChainError>,
{
    fn invoke(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ChainError> {
        (self.0)(tool_name, args)
    }
}

/// Blanket forwarding impl mirroring [`FmCompletion`]'s `&mut T`
/// helper. Lets tests retain ownership of a stateful invoker.
impl<T: ToolInvoker + ?Sized> ToolInvoker for &mut T {
    fn invoke(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ChainError> {
        (**self).invoke(tool_name, args)
    }
}

/// Errors surfaced by the chain loop. Most variants are *recoverable*
/// from the orchestrator's perspective — they map to specific
/// `LoopOutcome` paths rather than bubbling up as a top-level Result —
/// but they're modeled as a typed error so test scaffolding can
/// distinguish them precisely.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// FM returned a malformed structured-output payload that we
    /// couldn't parse into the expected shape. The string holds the
    /// raw response and the parser's complaint.
    #[error("malformed FM response: {0}")]
    Malformed(String),
    /// FM picked a tool that isn't in the registered set. The loop
    /// translates this into [`LoopOutcome::Escalate`] with the
    /// offending name in `reason`.
    #[error("FM picked unregistered tool: {0:?}")]
    UnknownTool(String),
    /// Translating a tool's schema failed. PDX-14's translator surfaces
    /// the unsupported keyword and field path; we route around the
    /// affected tool by escalating.
    #[error("tool schema translation failed: {0}")]
    Translation(#[from] TranslationError),
    /// The MCP tool itself returned an error. The loop tolerates up to
    /// [`ChainConfig::max_consecutive_failures`] of these in a row before
    /// escalating; the fact that we surface the raw message means
    /// non-FM providers can use it as context if the task is rerouted.
    #[error("tool invocation failed: {0}")]
    ToolFailed(String),
    /// FM is not available on this system. Should be caught earlier by
    /// the router but we propagate it cleanly anyway.
    #[error("Foundation Models bridge: {0}")]
    Bridge(#[from] crate::FoundationModelsError),
}

/// One observation in [`SessionState::completed_steps`]. Captured for
/// every loop iteration — successful or not — so the orchestrator can
/// hand the trail to the user (or the next provider, on escalation)
/// without losing the model's intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    /// Which tool the model chose. `None` means the model declined to
    /// call a tool and finished early — see [`StepRecord::final_answer`].
    pub tool: Option<String>,
    /// Raw arguments the model produced. Stored as JSON so we never
    /// have to round-trip through Swift to compare them.
    pub args: serde_json::Value,
    /// One-line natural-language reasoning the model emitted alongside
    /// the structured choice. Used in the per-step compressed summary.
    pub reasoning: String,
    /// Tool result, if the call succeeded. Empty string for
    /// failed/skipped steps — the failure is captured separately in
    /// `error`.
    pub result: serde_json::Value,
    /// Error text if the tool failed on this step. The loop tolerates a
    /// few of these in a row; persisting them keeps the eventual
    /// escalation message useful.
    pub error: Option<String>,
    /// Final natural-language answer the model produced when finishing
    /// without a further tool call. `Some` only on the terminal record
    /// when the model selected `Finish`.
    pub final_answer: Option<String>,
}

/// The loop's external state — explicitly *not* fed back into FM as
/// conversation history. The chain renderer composes a per-step prompt
/// from {goal, last result, compressed summary of prior steps}; the
/// rest stays here for the orchestrator to inspect when the loop
/// terminates or escalates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// The user's high-level goal — never mutated by the loop.
    pub goal: String,
    /// Steps the loop has executed so far, in order. Cleared only when
    /// a fresh `SessionState` is constructed; the loop appends to it.
    pub completed_steps: Vec<StepRecord>,
    /// Free-form key/value bag the orchestrator (or a future
    /// "remember" tool) can use to thread structured artefacts across
    /// steps without fattening the model's prompt. Today it's
    /// untouched by the loop; PDX-16 will populate it from tool
    /// results that match a known schema.
    pub current_artifacts: HashMap<String, serde_json::Value>,
}

impl SessionState {
    /// Fresh session for `goal` with no completed steps and no
    /// artefacts. The loop never constructs this for you — the caller
    /// owns the `SessionState` so that on `Escalate` they can hand the
    /// same value to the next provider.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            completed_steps: Vec::new(),
            current_artifacts: HashMap::new(),
        }
    }
}

/// Configuration for [`run_chain`]. Defaults match the values used by
/// PDX-16's planned router wiring.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Hard upper bound on iterations. Reaching this limit yields
    /// [`LoopOutcome::StepBoundExceeded`] with the partial state.
    pub max_steps: usize,
    /// Number of *consecutive* tool failures we'll tolerate before
    /// escalating. We use *consecutive* (not cumulative) so a flaky tool
    /// that occasionally fails doesn't poison the whole run; the
    /// counter resets on every success.
    pub max_consecutive_failures: usize,
    /// The set of tool names the model is allowed to call. Anything
    /// outside this set is treated as a capability miss → escalate.
    /// The orchestrator builds this from the live MCP tool catalog.
    pub registered_tools: Vec<String>,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_consecutive_failures: 2,
            registered_tools: Vec::new(),
        }
    }
}

/// Terminal outcome of a single [`run_chain`] invocation. The
/// orchestrator is expected to dispatch on this:
///
///   * [`LoopOutcome::Finished`]: deliver the final answer to the user;
///   * [`LoopOutcome::Escalate`]: rerun the task on a more capable
///     provider, optionally seeding it with `state` so it doesn't
///     redo the work FM already completed;
///   * [`LoopOutcome::StepBoundExceeded`]: surface the partial state
///     and let the user decide whether to extend the budget or
///     reroute.
#[derive(Debug)]
pub enum LoopOutcome {
    /// The model produced a final answer within the step budget.
    Finished {
        /// The natural-language answer FM emitted on the terminal
        /// `Finish` step. Always non-empty (the parser rejects empty
        /// `Finish` payloads upstream).
        answer: String,
        /// The full session state, including every step the loop
        /// executed. Useful for telemetry and as a transcript shown
        /// alongside the answer.
        state: SessionState,
    },
    /// The loop bailed out because of a capability miss. The
    /// orchestrator should pick a more capable provider and rerun.
    Escalate {
        /// Why we escalated (unknown tool, unsupported schema, repeated
        /// tool failures, malformed FM response). Logged verbatim by
        /// the orchestrator so the user can see *why* FM gave up.
        reason: EscalationReason,
        /// Partial state at the moment of escalation. Letting the next
        /// provider see this avoids the worst case of "FM tried for
        /// five steps then Claude redoes them all".
        state: SessionState,
    },
    /// The loop hit `max_steps` without finishing. Functionally similar
    /// to `Escalate` from the orchestrator's perspective but we keep
    /// the variant separate so telemetry can distinguish "FM stalled"
    /// from "FM hit a hard capability limit".
    StepBoundExceeded {
        /// Session state at the moment of the bound check.
        state: SessionState,
    },
}

/// Why the loop escalated. Mirrors the [`ChainError`] variants we treat
/// as escalations but with the concrete name/reason promoted to a
/// field so callers can branch without re-parsing the error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationReason {
    /// FM picked a tool that isn't in the registered set.
    UnknownTool(String),
    /// A tool's schema couldn't be translated to `@Generable` Swift,
    /// or the tool repeatedly returned errors. The string is the
    /// human-readable cause.
    ToolUnusable(String),
    /// FM returned malformed structured output more than once in a
    /// row. Indicates a prompt-design issue we can't fix in the loop.
    Malformed(String),
}

impl fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscalationReason::UnknownTool(name) => {
                write!(f, "FM requested unregistered tool {:?}", name)
            }
            EscalationReason::ToolUnusable(why) => write!(f, "tool unusable: {}", why),
            EscalationReason::Malformed(why) => write!(f, "malformed FM output: {}", why),
        }
    }
}

/// Parsed envelope for the first FM step. We embed the structured
/// output as JSON inside the model's text response — the production
/// caller pre-pends a `@Generable` schema preamble that nudges FM to
/// emit exactly this shape, but the loop is robust to FM emitting
/// extra prose around it (we extract the first valid JSON object).
#[derive(Debug, Clone, Deserialize)]
struct ToolChoiceJson {
    tool: String,
    #[serde(default)]
    args: serde_json::Value,
    #[serde(default)]
    reasoning: String,
}

/// Parsed envelope for follow-up steps. Either-or: the model picks one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContinueOrFinishJson {
    /// Pick another tool — payload mirrors `ToolChoiceJson`.
    Continue(ToolChoiceJson),
    /// Finish with a final answer.
    Finish { answer: String },
}

/// Drive the chain loop until it terminates. The function is
/// synchronous; if you're calling from an async context, wrap this in
/// `tokio::task::spawn_blocking` (the FM bridge underneath is blocking
/// too, so spawning is required regardless).
///
/// The caller owns `state` so escalation can hand the same value to
/// another provider without copying. On a successful `Finished` return
/// `state.completed_steps` will hold the full transcript.
pub fn run_chain<F, T>(
    config: &ChainConfig,
    state: &mut SessionState,
    mut fm: F,
    mut invoker: T,
) -> LoopOutcome
where
    F: FmCompletion,
    T: ToolInvoker,
{
    let mut consecutive_failures: usize = 0;
    let mut consecutive_malformed: usize = 0;

    for step_idx in 0..config.max_steps {
        let expected = if step_idx == 0 {
            ExpectedResponse::ToolChoice
        } else {
            ExpectedResponse::ContinueOrFinish
        };
        let prompt = render_prompt(state, expected, &config.registered_tools);

        // Step 1 of the iteration: FM picks the next action.
        let raw = match fm.complete(&prompt, expected) {
            Ok(r) => r,
            Err(ChainError::Bridge(e)) => {
                return LoopOutcome::Escalate {
                    reason: EscalationReason::Malformed(format!("FM bridge error: {}", e)),
                    state: std::mem::replace(state, SessionState::new("")),
                };
            }
            Err(e) => {
                return LoopOutcome::Escalate {
                    reason: EscalationReason::Malformed(e.to_string()),
                    state: std::mem::replace(state, SessionState::new("")),
                };
            }
        };

        let action = match parse_response(&raw, expected) {
            Ok(a) => {
                consecutive_malformed = 0;
                a
            }
            Err(why) => {
                consecutive_malformed += 1;
                if consecutive_malformed >= 2 {
                    return LoopOutcome::Escalate {
                        reason: EscalationReason::Malformed(why),
                        state: std::mem::replace(state, SessionState::new("")),
                    };
                }
                // Try once more with a fresh prompt; the loop will
                // re-render including the malformed-attempt summary
                // implicitly via `completed_steps` if we wrote one. We
                // intentionally do *not* write a record for the
                // malformed attempt — we have nothing useful to say
                // about it.
                continue;
            }
        };

        match action {
            ParsedAction::Finish { answer } => {
                state.completed_steps.push(StepRecord {
                    tool: None,
                    args: serde_json::Value::Null,
                    reasoning: String::new(),
                    result: serde_json::Value::Null,
                    error: None,
                    final_answer: Some(answer.clone()),
                });
                return LoopOutcome::Finished {
                    answer,
                    state: std::mem::replace(state, SessionState::new("")),
                };
            }
            ParsedAction::Tool {
                tool,
                args,
                reasoning,
            } => {
                // Capability miss: tool not in the registered set.
                if !config.registered_tools.iter().any(|t| t == &tool) {
                    state.completed_steps.push(StepRecord {
                        tool: Some(tool.clone()),
                        args,
                        reasoning,
                        result: serde_json::Value::Null,
                        error: Some(format!("tool {:?} not registered", tool)),
                        final_answer: None,
                    });
                    return LoopOutcome::Escalate {
                        reason: EscalationReason::UnknownTool(tool),
                        state: std::mem::replace(state, SessionState::new("")),
                    };
                }

                match invoker.invoke(&tool, &args) {
                    Ok(result) => {
                        consecutive_failures = 0;
                        state.completed_steps.push(StepRecord {
                            tool: Some(tool),
                            args,
                            reasoning,
                            result,
                            error: None,
                            final_answer: None,
                        });
                    }
                    Err(ChainError::Translation(t)) => {
                        // Per PDX-14: an Unsupported translation error
                        // is a hard capability miss for this tool. Don't
                        // retry; escalate with the field path so the
                        // orchestrator can route it elsewhere.
                        state.completed_steps.push(StepRecord {
                            tool: Some(tool),
                            args,
                            reasoning,
                            result: serde_json::Value::Null,
                            error: Some(t.to_string()),
                            final_answer: None,
                        });
                        return LoopOutcome::Escalate {
                            reason: EscalationReason::ToolUnusable(t.to_string()),
                            state: std::mem::replace(state, SessionState::new("")),
                        };
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let err_msg = e.to_string();
                        state.completed_steps.push(StepRecord {
                            tool: Some(tool),
                            args,
                            reasoning,
                            result: serde_json::Value::Null,
                            error: Some(err_msg.clone()),
                            final_answer: None,
                        });
                        if consecutive_failures > config.max_consecutive_failures {
                            return LoopOutcome::Escalate {
                                reason: EscalationReason::ToolUnusable(err_msg),
                                state: std::mem::replace(state, SessionState::new("")),
                            };
                        }
                    }
                }
            }
        }
    }

    LoopOutcome::StepBoundExceeded {
        state: std::mem::replace(state, SessionState::new("")),
    }
}

/// Internal flattening of the two parsed envelopes. Both `ToolChoice`
/// and `ContinueOrFinish::Continue` collapse to `Tool { ... }`; only
/// `Finish` is a distinct case. This lets the loop body use one match.
enum ParsedAction {
    Tool {
        tool: String,
        args: serde_json::Value,
        reasoning: String,
    },
    Finish {
        answer: String,
    },
}

/// Parse FM's raw text response into a [`ParsedAction`]. We extract
/// the first JSON object in the response rather than requiring the
/// whole response to be JSON, because real-world FM output sometimes
/// pads structured output with a leading explanatory sentence even
/// when the prompt asks for JSON only. Production callers should pin
/// the schema with a `@Generable` preamble so this fallback rarely
/// fires; tests exercise it deliberately.
fn parse_response(raw: &str, expected: ExpectedResponse) -> Result<ParsedAction, String> {
    let json_text = extract_json_object(raw)
        .ok_or_else(|| format!("no JSON object found in response: {:?}", raw))?;
    match expected {
        ExpectedResponse::ToolChoice => {
            let parsed: ToolChoiceJson = serde_json::from_str(json_text)
                .map_err(|e| format!("expected ToolChoice JSON: {} (raw: {:?})", e, json_text))?;
            if parsed.tool.is_empty() {
                return Err("ToolChoice.tool was empty".to_string());
            }
            Ok(ParsedAction::Tool {
                tool: parsed.tool,
                args: parsed.args,
                reasoning: parsed.reasoning,
            })
        }
        ExpectedResponse::ContinueOrFinish => {
            let parsed: ContinueOrFinishJson = serde_json::from_str(json_text).map_err(|e| {
                format!(
                    "expected ContinueOrFinish JSON: {} (raw: {:?})",
                    e, json_text
                )
            })?;
            match parsed {
                ContinueOrFinishJson::Continue(c) => {
                    if c.tool.is_empty() {
                        return Err("Continue.tool was empty".to_string());
                    }
                    Ok(ParsedAction::Tool {
                        tool: c.tool,
                        args: c.args,
                        reasoning: c.reasoning,
                    })
                }
                ContinueOrFinishJson::Finish { answer } => {
                    if answer.trim().is_empty() {
                        return Err("Finish.answer was empty".to_string());
                    }
                    Ok(ParsedAction::Finish { answer })
                }
            }
        }
    }
}

/// Find the first balanced `{...}` substring in `raw`. We deliberately
/// don't try to handle JSON inside string literals — for our envelope
/// shapes the model would have to emit a literal `{` inside a string
/// to fool this, which doesn't happen in practice with FM's structured
/// output. If it ever does, we'd see it as a parse error and the
/// double-malformed escalation path takes over.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let bytes = raw.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Render the per-step prompt. This is the heart of the
/// "minimisation" requirement: we feed FM only what it needs to
/// pick the next action — the goal, the registered tool list, the
/// most recent observation, and a one-line summary of every prior
/// step. We *don't* replay the full step history.
///
/// The output is plain text (markdown-ish) rather than JSON because
/// FM's instruction-following is noticeably better on natural prose.
/// PDX-16 will prepend a `@Generable` schema preamble at the call
/// site; that preamble plus this prompt fits inside FM's context.
fn render_prompt(
    state: &SessionState,
    expected: ExpectedResponse,
    tools: &[String],
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("# Goal\n");
    out.push_str(&state.goal);
    out.push_str("\n\n# Available tools\n");
    if tools.is_empty() {
        out.push_str("(none)\n");
    } else {
        for t in tools {
            out.push_str("- ");
            out.push_str(t);
            out.push('\n');
        }
    }

    let total = state.completed_steps.len();
    if total > 0 {
        // Compressed summary of all but the last step. We keep the most
        // recent step in full because that's the observation FM is
        // about to react to; older steps are reduced to one line
        // ("step N: tool=foo result=ok") so the model has continuity
        // without paying the prompt-size price.
        out.push_str("\n# Prior steps (compressed)\n");
        for (i, step) in state.completed_steps.iter().take(total - 1).enumerate() {
            out.push_str(&format!(
                "{}. {} -> {}\n",
                i + 1,
                step.tool.as_deref().unwrap_or("(finish)"),
                summarize_step_outcome(step),
            ));
        }
        out.push_str("\n# Last observation\n");
        let last = &state.completed_steps[total - 1];
        out.push_str(&format!(
            "tool: {}\nresult: {}\n",
            last.tool.as_deref().unwrap_or("(finish)"),
            truncate(&serde_json::to_string(&last.result).unwrap_or_default(), 800),
        ));
    }

    out.push_str("\n# Your turn\n");
    match expected {
        ExpectedResponse::ToolChoice => {
            out.push_str(
                "Pick the best next tool. Respond with a single JSON object: \
                {\"tool\": \"<name>\", \"args\": {...}, \"reasoning\": \"<one line>\"}.\n",
            );
        }
        ExpectedResponse::ContinueOrFinish => {
            out.push_str(
                "Either pick another tool or finish. Respond with one of:\n\
                {\"continue\": {\"tool\": \"<name>\", \"args\": {...}, \"reasoning\": \"<one line>\"}}\n\
                {\"finish\": {\"answer\": \"<final answer>\"}}\n",
            );
        }
    }

    // Enforce the soft prompt budget. We trim the compressed-summary
    // section first because losing older steps is the safest signal
    // to drop; the most recent observation and the schema preamble
    // (rendered later by the caller) are the load-bearing parts.
    if out.len() > PROMPT_BUDGET_CHARS {
        out = trim_to_budget(out, PROMPT_BUDGET_CHARS);
    }
    out
}

/// One-line summary of a step's outcome for the compressed-history
/// section. We deliberately don't include the args here — they're
/// usually a noisy reproduction of the goal that FM doesn't need to
/// re-read every step.
fn summarize_step_outcome(step: &StepRecord) -> String {
    if let Some(answer) = &step.final_answer {
        return format!("finished: {}", truncate(answer, 80));
    }
    if let Some(err) = &step.error {
        return format!("ERROR: {}", truncate(err, 80));
    }
    let preview = serde_json::to_string(&step.result).unwrap_or_default();
    format!("ok ({})", truncate(&preview, 80))
}

/// Truncate a string to `max` characters, appending `…` when truncation
/// happened. Operates on chars (not bytes) so multibyte content stays
/// valid; we don't care about being exact-length, only bounded.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Trim a rendered prompt to the budget by dropping the compressed
/// "Prior steps" section if it's there, and falling back to a flat
/// truncate-with-marker if even that isn't enough. We never drop the
/// "Last observation" or the "Your turn" tail.
fn trim_to_budget(prompt: String, budget: usize) -> String {
    if prompt.len() <= budget {
        return prompt;
    }
    // Drop the compressed-history block first if present.
    if let Some(start) = prompt.find("\n# Prior steps (compressed)\n") {
        if let Some(end) = prompt[start + 1..].find("\n# Last observation\n") {
            let absolute_end = start + 1 + end;
            let mut shorter = String::with_capacity(prompt.len());
            shorter.push_str(&prompt[..start]);
            shorter.push_str(&prompt[absolute_end..]);
            if shorter.len() <= budget {
                return shorter;
            }
            // Even without prior steps we're over budget; truncate the
            // observation body but keep the "Your turn" tail intact.
            return hard_truncate(shorter, budget);
        }
    }
    hard_truncate(prompt, budget)
}

/// Last-resort: keep the head and tail of the prompt and elide the
/// middle with a marker. We bias toward keeping the tail (which holds
/// the "Your turn" instructions) because losing it would yield nothing
/// useful from the model.
fn hard_truncate(prompt: String, budget: usize) -> String {
    if prompt.len() <= budget {
        return prompt;
    }
    let marker = "\n…[truncated]…\n";
    let keep = budget.saturating_sub(marker.len());
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    // Find char boundaries near our byte positions to avoid panicking
    // on multibyte content.
    let head = floor_char_boundary(&prompt, head_len);
    let tail_start = ceil_char_boundary(&prompt, prompt.len() - tail_len);
    let mut out = String::with_capacity(budget + marker.len());
    out.push_str(&prompt[..head]);
    out.push_str(marker);
    out.push_str(&prompt[tail_start..]);
    out
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a config with the registered tools you actually need so
    /// individual tests aren't responsible for tedious wiring.
    fn config_with(tools: &[&str]) -> ChainConfig {
        ChainConfig {
            max_steps: 5,
            max_consecutive_failures: 1,
            registered_tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Three-step happy path: tool, tool, finish. Verifies the loop
    /// advances through both `ToolChoice` and `ContinueOrFinish`
    /// responses, captures every result, and lands in `Finished`.
    #[test]
    fn three_step_success_path() {
        let config = config_with(&["search", "summarize"]);
        let mut state = SessionState::new("Find recent news on FM and summarize.");

        // Scripted FM responses, one per call.
        let scripted = vec![
            r#"{"tool": "search", "args": {"q": "Foundation Models"}, "reasoning": "look it up"}"#
                .to_string(),
            r#"{"continue": {"tool": "summarize", "args": {"text": "..."}, "reasoning": "compress"}}"#
                .to_string(),
            r#"{"finish": {"answer": "FM is Apple's on-device LLM."}}"#.to_string(),
        ];
        let mut iter = scripted.into_iter();
        let fm = move |_p: &str, _e: ExpectedResponse| -> Result<String, ChainError> {
            iter.next()
                .ok_or_else(|| ChainError::Malformed("ran out of scripted FM responses".into()))
        };

        let invoker = |tool: &str, _args: &serde_json::Value| -> Result<serde_json::Value, ChainError> {
            match tool {
                "search" => Ok(json!({"hits": ["a", "b"]})),
                "summarize" => Ok(json!({"summary": "FM is on-device."})),
                other => panic!("unexpected tool {:?}", other),
            }
        };

        let outcome = run_chain(&config, &mut state, FmFn::new(fm), ToolFn::new(invoker));
        match outcome {
            LoopOutcome::Finished { answer, state } => {
                assert_eq!(answer, "FM is Apple's on-device LLM.");
                // 2 tool steps + 1 finish marker
                assert_eq!(state.completed_steps.len(), 3);
                assert_eq!(state.completed_steps[0].tool.as_deref(), Some("search"));
                assert_eq!(state.completed_steps[1].tool.as_deref(), Some("summarize"));
                assert!(state.completed_steps[2].final_answer.is_some());
            }
            other => panic!("expected Finished, got {:?}", other),
        }
    }

    /// Capability miss: FM picks a tool that's not registered. The
    /// loop must escalate without invoking any tool, surfacing the
    /// offending name in the `EscalationReason`.
    #[test]
    fn escalates_on_unknown_tool() {
        let config = config_with(&["search"]);
        let mut state = SessionState::new("do the thing");

        let fm = |_p: &str, _e: ExpectedResponse| -> Result<String, ChainError> {
            Ok(r#"{"tool": "delete_database", "args": {}, "reasoning": "yolo"}"#.to_string())
        };
        let invoker = |_tool: &str, _args: &serde_json::Value| -> Result<serde_json::Value, ChainError> {
            panic!("invoker must not be called for unknown tools");
        };

        let outcome = run_chain(&config, &mut state, FmFn::new(fm), ToolFn::new(invoker));
        match outcome {
            LoopOutcome::Escalate { reason, state } => {
                assert!(matches!(reason, EscalationReason::UnknownTool(ref n) if n == "delete_database"));
                // The bad step is recorded so PDX-16 can surface it.
                assert_eq!(state.completed_steps.len(), 1);
                assert_eq!(
                    state.completed_steps[0].tool.as_deref(),
                    Some("delete_database")
                );
                assert!(state.completed_steps[0].error.is_some());
            }
            other => panic!("expected Escalate, got {:?}", other),
        }
    }

    /// Step-bound exhaustion: FM keeps calling tools forever. The
    /// loop must yield `StepBoundExceeded` with the partial state at
    /// exactly `max_steps` recorded steps.
    #[test]
    fn step_bound_exceeded() {
        let config = ChainConfig {
            max_steps: 3,
            max_consecutive_failures: 99,
            registered_tools: vec!["noop".to_string()],
        };
        let mut state = SessionState::new("loop forever");

        let fm = |_p: &str, e: ExpectedResponse| -> Result<String, ChainError> {
            // First call is ToolChoice, rest are ContinueOrFinish.
            // Always pick the noop tool so we never finish.
            Ok(match e {
                ExpectedResponse::ToolChoice => {
                    r#"{"tool": "noop", "args": {}, "reasoning": "go"}"#.to_string()
                }
                ExpectedResponse::ContinueOrFinish => {
                    r#"{"continue": {"tool": "noop", "args": {}, "reasoning": "go"}}"#.to_string()
                }
            })
        };
        let invoker =
            |_tool: &str, _args: &serde_json::Value| Ok(json!({"step": "ok"}));

        let outcome = run_chain(&config, &mut state, FmFn::new(fm), ToolFn::new(invoker));
        match outcome {
            LoopOutcome::StepBoundExceeded { state } => {
                assert_eq!(state.completed_steps.len(), 3);
                for step in &state.completed_steps {
                    assert_eq!(step.tool.as_deref(), Some("noop"));
                    assert!(step.error.is_none());
                }
            }
            other => panic!("expected StepBoundExceeded, got {:?}", other),
        }
    }

    /// Repeated tool failures escalate once `max_consecutive_failures`
    /// is exceeded. We set the threshold to 1 so the second failure
    /// triggers escalation; the first one is recorded but tolerated.
    #[test]
    fn escalates_on_repeated_tool_failures() {
        let config = ChainConfig {
            max_steps: 5,
            max_consecutive_failures: 1,
            registered_tools: vec!["flaky".to_string()],
        };
        let mut state = SessionState::new("test flaky tool");

        let fm = |_p: &str, e: ExpectedResponse| -> Result<String, ChainError> {
            Ok(match e {
                ExpectedResponse::ToolChoice => {
                    r#"{"tool": "flaky", "args": {}, "reasoning": "try"}"#.to_string()
                }
                ExpectedResponse::ContinueOrFinish => {
                    r#"{"continue": {"tool": "flaky", "args": {}, "reasoning": "try again"}}"#
                        .to_string()
                }
            })
        };
        let invoker = |_tool: &str, _args: &serde_json::Value| -> Result<serde_json::Value, ChainError> {
            Err(ChainError::ToolFailed("upstream 500".to_string()))
        };

        let outcome = run_chain(&config, &mut state, FmFn::new(fm), ToolFn::new(invoker));
        match outcome {
            LoopOutcome::Escalate { reason, state } => {
                assert!(matches!(reason, EscalationReason::ToolUnusable(_)));
                // Two failures recorded — the threshold-tolerant one
                // and the one that tipped us into escalation.
                assert_eq!(state.completed_steps.len(), 2);
                assert!(state
                    .completed_steps
                    .iter()
                    .all(|s| s.error.as_deref() == Some("tool invocation failed: upstream 500")));
            }
            other => panic!("expected Escalate, got {:?}", other),
        }
    }

    /// The renderer must keep the per-step prompt under
    /// `PROMPT_BUDGET_CHARS` even with a long step history. We seed a
    /// session with many large steps and assert the final prompt
    /// length, regardless of what the renderer chose to drop.
    #[test]
    fn prompt_minimisation_respects_budget() {
        let mut state = SessionState::new("a".repeat(500));
        // 50 steps at 1 KB of fake result each = 50 KB raw history;
        // the budget is 4 KB so trimming is mandatory.
        for i in 0..50 {
            state.completed_steps.push(StepRecord {
                tool: Some(format!("tool_{}", i)),
                args: json!({"i": i}),
                reasoning: "x".repeat(100),
                result: json!({"payload": "p".repeat(900)}),
                error: None,
                final_answer: None,
            });
        }
        let prompt = render_prompt(
            &state,
            ExpectedResponse::ContinueOrFinish,
            &["tool_0".to_string()],
        );
        assert!(
            prompt.len() <= PROMPT_BUDGET_CHARS,
            "rendered prompt was {} chars (> budget {})",
            prompt.len(),
            PROMPT_BUDGET_CHARS,
        );
        // The "Your turn" instruction tail must survive trimming —
        // without it the model has no schema to follow.
        assert!(prompt.contains("Your turn"));
    }

    /// The first step uses `ToolChoice`; subsequent steps use
    /// `ContinueOrFinish`. This is what lets PDX-16 build the right
    /// `@Generable` schema preamble at the call site. We use a
    /// struct-style FM impl here (rather than a closure) so the test
    /// can inspect the per-call `expected` history after the loop
    /// returns without fighting the borrow checker.
    #[test]
    fn expected_response_shape_changes_after_first_step() {
        let config = config_with(&["t"]);
        let mut state = SessionState::new("g");

        struct ScriptFm {
            calls: usize,
            seen: Vec<ExpectedResponse>,
        }
        impl FmCompletion for ScriptFm {
            fn complete(
                &mut self,
                _prompt: &str,
                expected: ExpectedResponse,
            ) -> Result<String, ChainError> {
                self.seen.push(expected);
                self.calls += 1;
                Ok(match self.calls {
                    1 => r#"{"tool": "t", "args": {}, "reasoning": "r"}"#.into(),
                    _ => r#"{"finish": {"answer": "done"}}"#.into(),
                })
            }
        }
        let mut script = ScriptFm {
            calls: 0,
            seen: Vec::new(),
        };
        let invoker = |_t: &str, _a: &serde_json::Value| Ok(json!(null));
        let _ = run_chain(&config, &mut state, &mut script, ToolFn::new(invoker));
        assert_eq!(script.seen.len(), 2);
        assert_eq!(script.seen[0], ExpectedResponse::ToolChoice);
        assert_eq!(script.seen[1], ExpectedResponse::ContinueOrFinish);
    }

    /// Malformed responses are tolerated once per consecutive run and
    /// escalate on the second consecutive failure. We script two
    /// non-JSON responses in a row and assert escalation with a
    /// `Malformed` reason.
    #[test]
    fn escalates_on_consecutive_malformed_responses() {
        let config = config_with(&["t"]);
        let mut state = SessionState::new("g");
        let fm = |_p: &str, _e: ExpectedResponse| -> Result<String, ChainError> {
            Ok("not json at all".to_string())
        };
        let invoker = |_t: &str, _a: &serde_json::Value| Ok(json!(null));
        let outcome = run_chain(&config, &mut state, FmFn::new(fm), ToolFn::new(invoker));
        assert!(matches!(
            outcome,
            LoopOutcome::Escalate {
                reason: EscalationReason::Malformed(_),
                ..
            }
        ));
    }

    /// `extract_json_object` must skip leading prose and grab the
    /// first balanced object. This is what lets us tolerate FM
    /// emitting "Here's my answer: {...}" instead of pure JSON.
    #[test]
    fn extracts_json_from_padded_output() {
        let raw = r#"Sure, here you go: {"tool":"x","args":{},"reasoning":"r"} thanks!"#;
        let extracted = extract_json_object(raw).expect("found object");
        assert_eq!(extracted, r#"{"tool":"x","args":{},"reasoning":"r"}"#);
    }

    /// Truncation is char-aware (no panics on multibyte content) and
    /// respects the requested character cap.
    #[test]
    fn truncate_is_char_aware() {
        let s = "äbc̈ëfg";
        let t = truncate(s, 3);
        assert_eq!(t.chars().count(), 4); // 3 + the ellipsis
    }

    /// `Finished` answers must be non-empty — the parser rejects
    /// empty `Finish.answer`.
    #[test]
    fn rejects_empty_finish_answer() {
        let r = parse_response(
            r#"{"finish": {"answer": "   "}}"#,
            ExpectedResponse::ContinueOrFinish,
        );
        assert!(r.is_err());
    }
}

