//! Context budgeting for the local model.
//!
//! Token counts come from the loaded model's tokenizer when the server offers one (llama-server
//! `/tokenize`, see `tokens.rs`). Otherwise they are estimated from text length (≈3.3
//! bytes/token), which is deliberately a little pessimistic: Qwen's tokenizer measured ≈3.7
//! chars/token on Python source.
//! A call needs: system prompt + spec + files + chat-template overhead + the reserved output
//! (`max_tokens`, which is also what we send in the request) + reasoning reserve.

use serde::{Deserialize, Serialize};

/// Per-message chat-template overhead (role markers, separators).
pub const MESSAGE_OVERHEAD: u32 = 16;
/// Fraction of the context kept free as a safety margin for estimation error.
pub const SAFETY_FRACTION: f64 = 0.05;
/// The margin with exact token counts: only the chat template and per-file headers are estimated.
pub const EXACT_SAFETY_FRACTION: f64 = 0.02;

pub fn safety_fraction(exact: bool) -> f64 {
    if exact { EXACT_SAFETY_FRACTION } else { SAFETY_FRACTION }
}

pub fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    // Bytes rather than chars: non-ASCII text tokenises into more tokens per character.
    ((text.len() as f64) / 3.3).ceil() as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitMode {
    Single,
    PerFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTokens {
    pub path: String,
    pub tokens: u32,
    /// True if the spec edits this file; false for read-only context files.
    pub target: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetInput {
    pub context_limit: u32,
    pub max_output: u32,
    pub reasoning_reserve: u32,
    pub system_tokens: u32,
    pub spec_tokens: u32,
    pub files: Vec<FileTokens>,
    /// Per-file spec size for split mode (same order as target files). Missing → spec_tokens.
    pub per_file_spec_tokens: Vec<(String, u32)>,
    /// True if the counts come from the model's tokenizer rather than the length estimate.
    #[serde(default)]
    pub exact: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallBudget {
    /// Target file for split mode; `None` for the single combined call.
    pub target: Option<String>,
    /// Read-only context files that fit alongside the target.
    pub context_files: Vec<String>,
    pub input_tokens: u32,
    pub total_tokens: u32,
    pub fits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetPlan {
    pub context_limit: u32,
    /// Usable tokens after the safety margin.
    pub usable: u32,
    pub reserved_output: u32,
    pub single: CallBudget,
    pub per_file: Vec<CallBudget>,
    pub recommended: SplitMode,
    pub warnings: Vec<String>,
    /// True if the token counts are exact (from the model's tokenizer).
    #[serde(default)]
    pub exact: bool,
}

impl BudgetPlan {
    pub fn fits_single(&self) -> bool {
        self.single.fits
    }
}

pub fn plan(input: &BudgetInput) -> BudgetPlan {
    let safety = safety_fraction(input.exact);
    let usable = ((input.context_limit as f64) * (1.0 - safety)).floor() as u32;
    let reserved = input.max_output + input.reasoning_reserve;
    let overhead = 2 * MESSAGE_OVERHEAD;
    let mut warnings = Vec::new();

    // Single call: everything together.
    let all_files: u32 = input.files.iter().map(|f| f.tokens + MESSAGE_OVERHEAD / 2).sum();
    let single_input = input.system_tokens + input.spec_tokens + all_files + overhead;
    let single_total = single_input + reserved;
    let single = CallBudget {
        target: None,
        context_files: input.files.iter().filter(|f| !f.target).map(|f| f.path.clone()).collect(),
        input_tokens: single_input,
        total_tokens: single_total,
        fits: single_total <= usable,
    };

    // Split: one call per target file, plus as many context files as fit (smallest first).
    let mut ctx_files: Vec<&FileTokens> = input.files.iter().filter(|f| !f.target).collect();
    ctx_files.sort_by_key(|f| f.tokens);
    let mut per_file = Vec::new();
    for f in input.files.iter().filter(|f| f.target) {
        let spec_t = input
            .per_file_spec_tokens
            .iter()
            .find(|(p, _)| p.eq_ignore_ascii_case(&f.path))
            .map(|(_, t)| *t)
            .unwrap_or(input.spec_tokens);
        let mut inp = input.system_tokens + spec_t + f.tokens + overhead + MESSAGE_OVERHEAD / 2;
        let mut included = Vec::new();
        for c in &ctx_files {
            let add = c.tokens + MESSAGE_OVERHEAD / 2;
            if inp + add + reserved <= usable {
                inp += add;
                included.push(c.path.clone());
            }
        }
        let total = inp + reserved;
        if total > usable {
            warnings.push(format!(
                "`{}` alone needs ≈{} tokens, more than the {} usable in this profile's context. Use a profile with a larger context, or narrow the spec.",
                f.path, total, usable
            ));
        }
        per_file.push(CallBudget {
            target: Some(f.path.clone()),
            context_files: included,
            input_tokens: inp,
            total_tokens: total,
            fits: total <= usable,
        });
    }

    let recommended = if single.fits || per_file.is_empty() {
        SplitMode::Single
    } else {
        SplitMode::PerFile
    };
    if !single.fits {
        warnings.insert(
            0,
            format!(
                "The whole job needs ≈{} tokens but the profile allows {} (of {} context, minus {}% safety margin). Splitting per file is recommended.",
                single_total,
                usable,
                input.context_limit,
                (safety * 100.0).round() as u32
            ),
        );
    }
    let dropped: usize = per_file.iter().map(|c| ctx_files.len() - c.context_files.len()).max().unwrap_or(0);
    if recommended == SplitMode::PerFile && dropped > 0 {
        warnings.push(format!(
            "In per-file mode, up to {dropped} read-only context file(s) are left out of some calls to stay within budget."
        ));
    }
    BudgetPlan {
        context_limit: input.context_limit,
        usable,
        reserved_output: reserved,
        single,
        per_file,
        recommended,
        warnings,
        exact: input.exact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(ctx: u32, files: Vec<(&str, u32, bool)>) -> BudgetInput {
        BudgetInput {
            context_limit: ctx,
            max_output: 4096,
            reasoning_reserve: 0,
            system_tokens: 500,
            spec_tokens: 1500,
            files: files
                .into_iter()
                .map(|(p, t, target)| FileTokens {
                    path: p.into(),
                    tokens: t,
                    target,
                })
                .collect(),
            per_file_spec_tokens: vec![],
            exact: false,
        }
    }

    #[test]
    fn estimate_is_length_based_and_pessimistic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens(&"x".repeat(330)), 100);
        // Multibyte text counts more.
        assert!(estimate_tokens("ééé") > estimate_tokens("eee"));
    }

    #[test]
    fn small_job_fits_single() {
        let p = plan(&input(32768, vec![("a.rs", 3000, true), ("b.rs", 2000, true)]));
        assert!(p.fits_single());
        assert_eq!(p.recommended, SplitMode::Single);
        assert!(p.warnings.is_empty());
        assert_eq!(p.usable, 31129);
        assert_eq!(p.single.total_tokens, 500 + 1500 + (3008 + 2008) + 32 + 4096);
    }

    #[test]
    fn large_job_recommends_split() {
        let p = plan(&input(32768, vec![("a.rs", 15000, true), ("b.rs", 14000, true)]));
        assert!(!p.fits_single());
        assert_eq!(p.recommended, SplitMode::PerFile);
        assert_eq!(p.per_file.len(), 2);
        assert!(p.per_file.iter().all(|c| c.fits));
        assert!(p.warnings[0].contains("Splitting per file is recommended"));
    }

    #[test]
    fn file_too_large_even_alone_is_flagged() {
        let p = plan(&input(32768, vec![("huge.rs", 40000, true), ("ok.rs", 1000, true)]));
        assert_eq!(p.recommended, SplitMode::PerFile);
        assert!(!p.per_file[0].fits);
        assert!(p.per_file[1].fits);
        assert!(p.warnings.iter().any(|w| w.contains("`huge.rs` alone")));
    }

    #[test]
    fn context_files_are_packed_smallest_first_in_split_mode() {
        let p = plan(&input(
            32768,
            vec![
                ("a.rs", 20000, true),
                ("b.rs", 20000, true),
                ("big_ctx.rs", 6000, false),
                ("small_ctx.rs", 1000, false),
            ],
        ));
        assert_eq!(p.recommended, SplitMode::PerFile);
        assert_eq!(p.per_file[0].context_files, vec!["small_ctx.rs".to_string()]);
        assert!(p.warnings.iter().any(|w| w.contains("context file")));
    }

    #[test]
    fn reasoning_reserve_counts_against_budget() {
        let mut i = input(16384, vec![("a.rs", 9000, true)]);
        assert!(plan(&i).fits_single());
        i.reasoning_reserve = 2048;
        assert!(!plan(&i).fits_single());
    }

    #[test]
    fn exact_counts_use_a_smaller_margin() {
        let mut i = input(32768, vec![("a.rs", 3000, true)]);
        let est = plan(&i);
        i.exact = true;
        let ex = plan(&i);
        assert!(!est.exact && ex.exact);
        assert_eq!(est.usable, 31129);
        assert_eq!(ex.usable, 32112);
        // A job just over the estimated budget fits with exact counts.
        let mut j = input(32768, vec![("a.rs", 31129 - 500 - 1500 - 32 - 4096 - 8 + 100, true)]);
        assert!(!plan(&j).fits_single());
        j.exact = true;
        assert!(plan(&j).fits_single());
    }

    #[test]
    fn per_file_spec_sizes_are_used() {
        let mut i = input(32768, vec![("a.rs", 15000, true), ("b.rs", 14000, true)]);
        i.per_file_spec_tokens = vec![("a.rs".into(), 300)];
        let p = plan(&i);
        assert_eq!(p.per_file[0].input_tokens, 500 + 300 + 15000 + 32 + 8);
        assert_eq!(p.per_file[1].input_tokens, 500 + 1500 + 14000 + 32 + 8);
    }
}
